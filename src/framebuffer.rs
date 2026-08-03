use crate::{Error, Result};

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
        &mut self.rgba
    }

    pub fn resize(&mut self, width: u16, height: u16) -> Result<()> {
        let replacement = Self::new(width, height)?;
        *self = replacement;
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
        let offset = (usize::from(y) * usize::from(self.width) + usize::from(x)) * 4;
        self.rgba[offset..offset + 4].copy_from_slice(&rgba);
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
        let len = usize::from(rect.width)
            .checked_mul(usize::from(rect.height))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(Error::LimitExceeded("CopyRect size"))?;
        let mut copy = Vec::with_capacity(len);
        for row in 0..rect.height {
            let start =
                (usize::from(src_y + row) * usize::from(self.width) + usize::from(src_x)) * 4;
            let row_len = usize::from(rect.width) * 4;
            copy.extend_from_slice(&self.rgba[start..start + row_len]);
        }
        for row in 0..rect.height {
            let source_start = usize::from(row) * usize::from(rect.width) * 4;
            let destination_start =
                (usize::from(rect.y + row) * usize::from(self.width) + usize::from(rect.x)) * 4;
            let row_len = usize::from(rect.width) * 4;
            self.rgba[destination_start..destination_start + row_len]
                .copy_from_slice(&copy[source_start..source_start + row_len]);
        }
        Ok(())
    }
}
