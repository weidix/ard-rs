//! Platform decode backends for the AVC media stream (encoding 1010).
//!
//! The core crate turns UDP/SRTP/RTP into whole access units; these backends
//! turn access units into displayable native YUV frames. Only macOS is implemented
//! today (VideoToolbox); Windows (MFT) is a follow-up.

#[cfg(target_os = "macos")]
pub mod vt;

#[cfg(target_os = "macos")]
pub mod pipeline;

#[cfg(target_os = "macos")]
pub use pipeline::spawn_avc_video_pipeline;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YuvRange {
    Video,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YuvMatrix {
    Bt601,
    Bt709,
    Bt2020,
}

/// One decoded NV12 slice returned by VideoProcessing.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct DecodedSlice {
    pub width: u32,
    pub height: u32,
    /// Luma plane, one byte per sample, tightly packed and row-major.
    pub y_plane: Vec<u8>,
    /// Interleaved CbCr plane, two bytes per 2x2 luma block, tightly packed.
    pub uv_plane: Vec<u8>,
    pub range: YuvRange,
    pub matrix: YuvMatrix,
}

#[derive(Debug, Clone)]
pub struct DecodedSliceUpdate {
    pub slice_index: usize,
    pub y_origin: u32,
    pub y_rows: u32,
    pub uv_origin: u32,
    pub uv_rows: u32,
    pub pixels: DecodedSlice,
}

/// One display boundary containing only the native NV12 slices that changed.
/// The renderer uploads each update directly into its persistent two-plane
/// textures, so no CPU-side full-frame composition is required.
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    /// Approximate encoded video payload bytes represented by this frame.
    pub encoded_bytes: usize,
    pub range: YuvRange,
    pub matrix: YuvMatrix,
    pub updates: Vec<DecodedSliceUpdate>,
}

impl DecodedFrame {
    pub fn merge_older_updates(&mut self, older: Self) {
        if self.width != older.width
            || self.height != older.height
            || self.range != older.range
            || self.matrix != older.matrix
        {
            return;
        }
        for update in older.updates {
            if !self
                .updates
                .iter()
                .any(|current| current.slice_index == update.slice_index)
            {
                self.updates.push(update);
            }
        }
        self.updates.sort_by_key(|update| update.slice_index);
        self.encoded_bytes = self.encoded_bytes.saturating_add(older.encoded_bytes);
    }
}
