use crate::{Error, PixelFormat, Result};

/// Describes the pixel layout retained by the core package.
///
/// The core always retains the negotiated RFB byte layout. Converting those
/// bytes to a presentation or texture format belongs to the consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FramebufferFormat {
    Native(PixelFormat),
}

impl FramebufferFormat {
    pub fn bytes_per_pixel(self) -> Result<usize> {
        match self {
            Self::Native(format) => format.bytes_per_pixel(),
        }
    }

    pub fn native_pixel_format(self) -> Option<PixelFormat> {
        match self {
            Self::Native(format) => Some(format),
        }
    }
}

/// Decoded framebuffer storage owned by the core package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Framebuffer {
    width: u16,
    height: u16,
    format: FramebufferFormat,
    pixels: Vec<u8>,
}

impl Framebuffer {
    /// Creates a framebuffer using the default 32-bit RFB layout.
    pub fn new(width: u16, height: u16) -> Result<Self> {
        Self::new_native(width, height, PixelFormat::XRGB8888)
    }

    /// Creates a framebuffer that retains decoded pixels in the supplied RFB
    /// format without converting them to a presentation format.
    pub fn new_native(width: u16, height: u16, pixel_format: PixelFormat) -> Result<Self> {
        Self::new_with_format(width, height, FramebufferFormat::Native(pixel_format))
    }

    pub fn new_with_format(width: u16, height: u16, format: FramebufferFormat) -> Result<Self> {
        let len = Self::byte_len(width, height, format)?;
        let FramebufferFormat::Native(pixel_format) = format;
        pixel_format.validate()?;
        Ok(Self {
            width,
            height,
            format,
            pixels: vec![0; len],
        })
    }

    /// Creates dimension-only storage for GPU-native decoding. Pixel storage
    /// is allocated lazily if a CPU-output encoding writes pixels.
    pub(crate) fn new_metadata_with_format(
        width: u16,
        height: u16,
        format: FramebufferFormat,
    ) -> Result<Self> {
        let _ = Self::byte_len(width, height, format)?;
        let FramebufferFormat::Native(pixel_format) = format;
        pixel_format.validate()?;
        Ok(Self {
            width,
            height,
            format,
            pixels: Vec::new(),
        })
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    pub fn format(&self) -> FramebufferFormat {
        self.format
    }

    /// Returns the RFB pixel layout used by [`Self::pixels`].
    pub fn pixel_format(&self) -> PixelFormat {
        self.native_pixel_format()
            .expect("core framebuffers always use a native pixel format")
    }

    pub fn native_pixel_format(&self) -> Option<PixelFormat> {
        self.format.native_pixel_format()
    }

    /// Returns the stored pixel bytes in the selected framebuffer format.
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    pub fn pixels_mut(&mut self) -> &mut [u8] {
        self.ensure_pixels();
        &mut self.pixels
    }

    /// Returns the retained RFB pixel bytes.
    pub fn native(&self) -> Option<&[u8]> {
        Some(&self.pixels)
    }

    pub fn resize(&mut self, width: u16, height: u16) -> Result<()> {
        let format = self.format;
        let has_storage = !self.pixels.is_empty();
        *self = if has_storage {
            Self::new_with_format(width, height, format)?
        } else {
            Self::new_metadata_with_format(width, height, format)?
        };
        Ok(())
    }

    pub(crate) fn validate_rect(&self, rect: &crate::Rectangle) -> Result<()> {
        let right = rect
            .x
            .checked_add(rect.width)
            .ok_or(Error::Invalid("rectangle x overflow"))?;
        let bottom = rect
            .y
            .checked_add(rect.height)
            .ok_or(Error::Invalid("rectangle y overflow"))?;
        if right > self.width || bottom > self.height {
            return Err(Error::Invalid("rectangle is outside the framebuffer"));
        }
        Ok(())
    }

    pub(crate) fn validate_pixel_format(&self, pixel_format: PixelFormat) -> Result<()> {
        if self.pixel_format() != pixel_format {
            return Err(Error::Invalid(
                "framebuffer pixel format differs from decoder pixel format",
            ));
        }
        Ok(())
    }

    pub(crate) fn set_pixel(&mut self, x: u16, y: u16, rgba: [u8; 4]) {
        self.ensure_pixels();
        let offset = self.pixel_offset(x, y);
        let FramebufferFormat::Native(pixel_format) = self.format;
        pixel_format
            .encode_pixel(rgba, &mut self.pixels[offset..])
            .expect("framebuffer pixel format was validated at construction");
    }

    pub(crate) fn set_pixel_bytes(
        &mut self,
        x: u16,
        y: u16,
        source_format: PixelFormat,
        bytes: &[u8],
    ) -> Result<()> {
        let source_bpp = source_format.bytes_per_pixel()?;
        if bytes.len() < source_bpp {
            return Err(Error::NeedMore {
                needed: source_bpp,
                available: bytes.len(),
            });
        }
        if self.native_pixel_format() != Some(source_format) {
            return Err(Error::Invalid(
                "framebuffer pixel format differs from decoder pixel format",
            ));
        }
        self.ensure_pixels();
        let offset = self.pixel_offset(x, y);
        self.pixels[offset..offset + source_bpp].copy_from_slice(&bytes[..source_bpp]);
        Ok(())
    }

    pub(crate) fn set_ycbcr(&mut self, x: u16, y: u16, sample: [u8; 3]) {
        self.set_pixel(x, y, ycbcr_to_rgba(sample));
    }

    pub(crate) fn copy_rect(
        &mut self,
        rect: &crate::Rectangle,
        src_x: u16,
        src_y: u16,
    ) -> Result<()> {
        let source = crate::Rectangle {
            x: src_x,
            y: src_y,
            width: rect.width,
            height: rect.height,
            encoding: rect.encoding,
        };
        self.validate_rect(rect)?;
        self.validate_rect(&source)?;
        self.ensure_pixels();
        let bytes_per_pixel = self.format.bytes_per_pixel()?;
        let row_len = usize::from(rect.width)
            .checked_mul(bytes_per_pixel)
            .ok_or(Error::LimitExceeded("CopyRect size"))?;
        let _len = row_len
            .checked_mul(usize::from(rect.height))
            .ok_or(Error::LimitExceeded("CopyRect size"))?;

        // `copy_within` is memmove-like, so each row remains overlap-safe
        // without allocating a second rectangle-sized pixel buffer. Copy
        // rows in the direction that keeps vertically overlapping rows from
        // overwriting source data that has not been read yet.
        let source_y = usize::from(src_y);
        let destination_y = usize::from(rect.y);
        let source_x = usize::from(src_x) * bytes_per_pixel;
        let destination_x = usize::from(rect.x) * bytes_per_pixel;
        let framebuffer_row_len = usize::from(self.width) * bytes_per_pixel;
        let rows = usize::from(rect.height);
        let mut copy_row = |row: usize| {
            let source_start = (source_y + row) * framebuffer_row_len + source_x;
            let destination_start = (destination_y + row) * framebuffer_row_len + destination_x;
            self.pixels
                .copy_within(source_start..source_start + row_len, destination_start);
        };
        if destination_y > source_y {
            for row in (0..rows).rev() {
                copy_row(row);
            }
        } else {
            for row in 0..rows {
                copy_row(row);
            }
        }
        Ok(())
    }

    fn ensure_pixels(&mut self) {
        if self.pixels.is_empty() && self.width != 0 && self.height != 0 {
            let len = Self::byte_len(self.width, self.height, self.format)
                .expect("framebuffer dimensions were validated at construction");
            self.pixels.resize(len, 0);
        }
    }

    fn pixel_offset(&self, x: u16, y: u16) -> usize {
        (usize::from(y) * usize::from(self.width) + usize::from(x))
            * self
                .format
                .bytes_per_pixel()
                .expect("framebuffer pixel format was validated at construction")
    }

    fn byte_len(width: u16, height: u16, format: FramebufferFormat) -> Result<usize> {
        usize::from(width)
            .checked_mul(usize::from(height))
            .and_then(|pixels| pixels.checked_mul(format.bytes_per_pixel().ok()?))
            .ok_or(Error::LimitExceeded("framebuffer size"))
    }
}

fn ycbcr_to_rgba(sample: [u8; 3]) -> [u8; 4] {
    let y = i32::from(sample[0]);
    let cb = i32::from(sample[1]) - 128;
    let cr = i32::from(sample[2]) - 128;
    let red = y + ((91_881 * cr + 32_768) >> 16);
    let green = y + ((32_768 - 22_554 * cb - 46_802 * cr) >> 16);
    let blue = y + ((116_130 * cb + 32_768) >> 16);
    [
        red.clamp(0, 255) as u8,
        green.clamp(0, 255) as u8,
        blue.clamp(0, 255) as u8,
        255,
    ]
}
