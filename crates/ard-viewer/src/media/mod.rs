//! Platform decode backends for the AVC media stream (encoding 1010).
//!
//! The core crate turns UDP/SRTP/RTP into whole access units; these backends
//! turn access units into displayable RGBA frames. Only macOS is implemented
//! today (VideoToolbox); Windows (MFT) is a follow-up.

#[cfg(target_os = "macos")]
pub mod vt;

#[cfg(target_os = "macos")]
pub mod pipeline;

#[cfg(target_os = "macos")]
pub use pipeline::{spawn_avc_video_pipeline, spawn_avc_video_pipeline_with_config};

/// A decoded frame ready for the viewer's RGBA display path.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    /// Approximate encoded video payload bytes represented by this frame.
    /// This is used for the viewer's AVC traffic meter; it excludes RTP and
    /// SRTP headers.
    pub encoded_bytes: usize,
    /// RGBA8, tightly packed, row-major.
    pub rgba: Vec<u8>,
}
