//! Platform decode backends for the AVC media stream (encoding 1010).
//!
//! The core crate turns UDP/SRTP/RTP into whole access units; these backends
//! turn access units into displayable native YUV frames. macOS uses
//! VideoToolbox and Windows uses Media Foundation Transforms (MFT).

use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
pub mod vt;

#[cfg(target_os = "windows")]
pub mod mft;

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub mod pipeline;

#[cfg(any(target_os = "macos", target_os = "windows"))]
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
    /// Monotonic timestamps for the live UDP/decode path. Synthetic and
    /// non-AVC frames deliberately leave this unset.
    pub timing: Option<AvcFrameTiming>,
}

#[derive(Debug, Clone, Copy)]
pub struct AvcFrameTiming {
    pub first_packet_received_at: Instant,
    pub first_access_unit_completed_at: Instant,
    pub batch_released_at: Instant,
    pub decoded_at: Instant,
    pub negotiated_dimensions: Option<(u32, u32)>,
}

impl AvcFrameTiming {
    pub fn packet_reassembly_duration(self) -> Duration {
        self.first_access_unit_completed_at
            .saturating_duration_since(self.first_packet_received_at)
    }

    pub fn batch_holdback(self) -> Duration {
        self.batch_released_at
            .saturating_duration_since(self.first_access_unit_completed_at)
    }

    pub fn decode_duration(self) -> Duration {
        self.decoded_at
            .saturating_duration_since(self.batch_released_at)
    }

    pub fn receive_to_decode(self) -> Duration {
        self.decoded_at
            .saturating_duration_since(self.first_packet_received_at)
    }
}

/// Platform-neutral outcome for one submitted compressed access unit.
#[derive(Debug)]
pub(crate) struct DecodedOutput {
    pub(crate) stream_index: usize,
    pub(crate) timestamp: u32,
    pub(crate) submission: u64,
    pub(crate) encoded_bytes: usize,
    pub(crate) status: i32,
    pub(crate) info_flags: u32,
    pub(crate) conversion_error: Option<String>,
    pub(crate) frame: Option<DecodedSlice>,
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
        if self.timing.is_none() {
            self.timing = older.timing;
        }
    }
}
