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

/// Colour primaries the decoded planes are encoded in.
///
/// Apple's media stream carries a full colour description in the HEVC VUI. A
/// Mac screen is commonly tagged Display P3 (`colour_primaries = 12`) while the
/// transfer function stays sRGB, so the planes are P3-encoded even though the
/// luma/chroma matrix is BT.709. Presenting P3 numbers as sRGB shifts the whole
/// picture — measured on a real device capture, the red channel of a flat
/// desktop background moves by ~39 of 255. The native client builds a
/// `CGColorSpace` from the buffer attachments
/// (`CVImageBufferCreateColorSpaceFromAttachments` in AVConference) exactly so
/// this does not happen.
// Non-macOS decode backends expose no primaries tag, so they only ever carry
// the default variant even though the renderer still uses the conversion.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum YuvPrimaries {
    /// Identity: the planes already carry sRGB/Rec.709 primaries.
    #[default]
    Bt709,
    /// Display P3, D65 white point.
    P3D65,
    /// Rec.2020 / BT.2100 primaries.
    Bt2020,
}

impl YuvPrimaries {
    /// Linear-light matrix that converts this primary set's RGB into the
    /// sRGB/Rec.709 primaries of a presentation surface.
    ///
    /// Returned row-major; rows are the sRGB R, G and B outputs. The presenter
    /// deliberately does not apply this: the reference client shows the decoded
    /// planes' numbers unchanged (see `session_renderer::yuv_conversion`), and a
    /// recording carries the same numbers with the stream's primaries attached
    /// as its colour tag. The matrix stays as the documented conversion, pinned
    /// by the tests below.
    #[allow(dead_code)]
    pub fn to_linear_srgb(self) -> [[f32; 3]; 3] {
        match self {
            Self::Bt709 => [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            Self::P3D65 => [
                [1.224_94, -0.224_94, 0.0],
                [-0.042_056, 1.042_056, 0.0],
                [-0.019_638, -0.078_636, 1.098_274],
            ],
            Self::Bt2020 => [
                [1.660_491, -0.587_641, -0.072_850],
                [-0.124_55, 1.132_9, -0.008_35],
                [-0.018_151, -0.100_579, 1.118_73],
            ],
        }
    }
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
    pub primaries: YuvPrimaries,
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
    pub primaries: YuvPrimaries,
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
            || self.primaries != older.primaries
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

#[cfg(test)]
mod tests {
    use super::YuvPrimaries;

    /// A D65 white point is shared by sRGB, Display P3 and Rec.2020, so every
    /// primaries conversion must leave neutral colours untouched. A matrix that
    /// does not sum to one per row is the classic way a colour-space fix turns
    /// into a whole-frame tint.
    #[test]
    fn primaries_conversion_preserves_d65_white() {
        for primaries in [
            YuvPrimaries::Bt709,
            YuvPrimaries::P3D65,
            YuvPrimaries::Bt2020,
        ] {
            for row in primaries.to_linear_srgb() {
                let sum: f32 = row.iter().sum();
                assert!(
                    (sum - 1.0).abs() < 1.0e-5,
                    "{primaries:?} row {row:?} sums to {sum}"
                );
            }
        }
    }

    #[test]
    fn bt709_primaries_are_the_identity() {
        assert_eq!(
            YuvPrimaries::Bt709.to_linear_srgb(),
            [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]
        );
    }

    /// Display P3's red primary sits outside the sRGB gamut, so its linear
    /// sRGB coordinates have to carry negative green and blue components. A
    /// matrix that maps it to something positive is not a P3 conversion.
    #[test]
    fn display_p3_red_needs_negative_srgb_components() {
        let matrix = YuvPrimaries::P3D65.to_linear_srgb();
        let red = [matrix[0][0], matrix[1][0], matrix[2][0]];
        assert!(red[0] > 1.2, "P3 red must exceed the sRGB gamut: {red:?}");
        assert!(red[1] < 0.0 && red[2] < 0.0, "P3 red leaves sRGB: {red:?}");
        assert_eq!(YuvPrimaries::P3D65.to_linear_srgb()[0][2], 0.0);
    }
}
