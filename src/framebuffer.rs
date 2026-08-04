use crate::{Error, Result};

/// Compatibility framebuffer used by the general-purpose decoder path.
/// GPU-native MVS decoding bypasses this storage entirely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Framebuffer {
    width: u16,
    height: u16,
    rgba: Vec<u8>,
}

impl Framebuffer {
    pub fn new(width: u16, height: u16) -> Result<Self> {
        let len = usize::from(width)
            .checked_mul(usize::from(height))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(Error::LimitExceeded("framebuffer size"))?;
        Ok(Self {
            width,
            height,
            rgba: vec![0; len],
        })
    }

    /// Creates a dimension-only framebuffer for GPU-native decoding. Pixel
    /// storage is allocated lazily only if a non-MVS compatibility encoding
    /// actually writes CPU pixels.
    pub(crate) fn new_metadata(width: u16, height: u16) -> Result<Self> {
        let _ = usize::from(width)
            .checked_mul(usize::from(height))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(Error::LimitExceeded("framebuffer size"))?;
        Ok(Self {
            width,
            height,
            rgba: Vec::new(),
        })
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }

    pub fn rgba_mut(&mut self) -> &mut [u8] {
        self.ensure_pixels();
        &mut self.rgba
    }

    pub fn resize(&mut self, width: u16, height: u16) -> Result<()> {
        *self = if self.rgba.is_empty() {
            Self::new_metadata(width, height)?
        } else {
            Self::new(width, height)?
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

    pub(crate) fn set_pixel(&mut self, x: u16, y: u16, rgba: [u8; 4]) {
        self.ensure_pixels();
        let offset = (usize::from(y) * usize::from(self.width) + usize::from(x)) * 4;
        self.rgba[offset..offset + 4].copy_from_slice(&rgba);
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
        let row_len = usize::from(rect.width)
            .checked_mul(4)
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
        let source_x = usize::from(src_x) * 4;
        let destination_x = usize::from(rect.x) * 4;
        let framebuffer_row_len = usize::from(self.width) * 4;
        let rows = usize::from(rect.height);
        let mut copy_row = |row: usize| {
            let source_start = (source_y + row) * framebuffer_row_len + source_x;
            let destination_start = (destination_y + row) * framebuffer_row_len + destination_x;
            self.rgba
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
        if self.rgba.is_empty() && self.width != 0 && self.height != 0 {
            self.rgba
                .resize(usize::from(self.width) * usize::from(self.height) * 4, 0);
        }
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
