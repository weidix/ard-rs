//! Hardware H.264 encoders used by the recorder.
//!
//! The recorder feeds the backends exactly the frames the viewer presents, so a
//! backend only has to turn one image into one access unit and report the
//! parameter sets the MP4 track needs. macOS uses VideoToolbox and Windows uses
//! the Media Foundation H.264 encoder; both are the same system encoders the
//! native Screen Sharing client uses for its own recordings.

use std::time::Duration;

#[cfg(target_os = "windows")]
mod mft;
#[cfg(target_os = "macos")]
mod vt;

use super::timeline_units;
use crate::media::{YuvMatrix, YuvPrimaries, YuvRange};

/// One encoded access unit in AVCC form (each NAL prefixed by its 4-byte length),
/// which is exactly how an `avcC`-described MP4 track stores samples.
#[derive(Debug, Clone)]
pub(crate) struct EncodedSample {
    pub bytes: Vec<u8>,
    /// The access unit starts a new prediction chain and may be seeked to.
    pub is_sync: bool,
    /// Presentation time in track timescale units.
    pub pts: u64,
    /// How long this frame stays on screen, in track timescale units.
    pub duration: u32,
}

/// H.264 parameter sets required by the container's configuration box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParameterSets {
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EncoderSettings {
    pub width: u32,
    pub height: u32,
    /// Target average bitrate in bits per second.
    pub bitrate: u32,
    /// Expected presentation rate, used as the encoder's rate-control hint.
    pub frame_rate_hint: u32,
    /// Colour primaries of the frames that will be submitted. The recorder
    /// passes the viewer's decoded planes through without a colour conversion,
    /// so the recording must describe the primaries those planes carry.
    pub primaries: YuvPrimaries,
}

/// The image formats a backend can be handed.
#[derive(Debug, Clone, Copy)]
pub(crate) enum SourceFrame<'a> {
    /// Packed 8-bit BGRA, the format VideoToolbox and Media Foundation both
    /// accept directly for screen content.
    Bgra { stride: usize, bytes: &'a [u8] },
    /// Biplanar 8-bit 4:2:0 with interleaved chroma, passed through without any
    /// colour conversion so an AVC session records the decoded planes verbatim.
    Nv12 {
        y_stride: usize,
        y: &'a [u8],
        uv_stride: usize,
        uv: &'a [u8],
        range: YuvRange,
        matrix: YuvMatrix,
    },
}

/// Measures how long each frame stays on screen.
///
/// A recording is variable-rate by construction: frames are captured when the
/// viewer presents them, so a frame's duration is only known once the next one
/// arrives. This holds the previous timestamp so the encoder can stamp the
/// sample it already holds.
#[derive(Debug, Default)]
pub(crate) struct FrameTimeline {
    last_pts: Option<Duration>,
}

impl FrameTimeline {
    /// Duration to stamp on the previously held frame, given the next frame.
    pub fn advance(&mut self, pts: Duration) -> Option<u32> {
        let previous = self.last_pts.replace(pts)?;
        Some(timeline_units(pts.saturating_sub(previous)))
    }

    /// Duration of the frame that is still on screen when the take ends.
    pub fn finish(&mut self, now: Duration) -> Option<u32> {
        let previous = self.last_pts.take()?;
        Some(timeline_units(now.saturating_sub(previous)))
    }
}

enum Backend {
    #[cfg(target_os = "macos")]
    VideoToolbox(vt::VideoToolboxEncoder),
    #[cfg(target_os = "windows")]
    MediaFoundation(mft::MediaFoundationEncoder),
    /// No system encoder on this platform. The recorder reports this as a
    /// configuration error instead of silently writing nothing.
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    Unsupported,
}

#[cfg(target_os = "macos")]
fn create_backend(settings: EncoderSettings) -> Result<Backend, String> {
    Ok(Backend::VideoToolbox(vt::VideoToolboxEncoder::new(
        settings,
    )?))
}

#[cfg(target_os = "windows")]
fn create_backend(settings: EncoderSettings) -> Result<Backend, String> {
    Ok(Backend::MediaFoundation(mft::MediaFoundationEncoder::new(
        settings,
    )?))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn create_backend(_settings: EncoderSettings) -> Result<Backend, String> {
    Err(UNSUPPORTED_PLATFORM.into())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const UNSUPPORTED_PLATFORM: &str = "当前平台没有系统 H.264 编码器，无法录制会话";

/// Whether this build can encode recordings at all.
pub(crate) const fn supported() -> bool {
    cfg!(any(target_os = "macos", target_os = "windows"))
}

pub(crate) struct H264Encoder {
    settings: EncoderSettings,
    backend: Backend,
}

impl H264Encoder {
    pub fn new(settings: EncoderSettings) -> Result<Self, String> {
        Ok(Self {
            settings,
            backend: create_backend(settings)?,
        })
    }

    /// Settings this encoder was created with. The recorder compares them with
    /// each captured frame's layout to decide whether it still belongs to the
    /// current segment.
    pub fn settings(&self) -> EncoderSettings {
        self.settings
    }

    /// Encode one presented frame. The frame is held until its duration is
    /// known, so output lags the captured timeline by exactly one frame.
    pub fn push(&mut self, frame: SourceFrame<'_>, pts: Duration) -> Result<(), String> {
        match &mut self.backend {
            #[cfg(target_os = "macos")]
            Backend::VideoToolbox(encoder) => encoder.push(frame, pts),
            #[cfg(target_os = "windows")]
            Backend::MediaFoundation(encoder) => encoder.push(frame, pts),
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            Backend::Unsupported => {
                let _ = (frame, pts);
                Err(UNSUPPORTED_PLATFORM.into())
            }
        }
    }

    /// Flush the held frame, drain the encoder, and stop accepting input.
    pub fn finish(&mut self, now: Duration) -> Result<(), String> {
        match &mut self.backend {
            #[cfg(target_os = "macos")]
            Backend::VideoToolbox(encoder) => encoder.finish(now),
            #[cfg(target_os = "windows")]
            Backend::MediaFoundation(encoder) => encoder.finish(now),
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            Backend::Unsupported => {
                let _ = now;
                Ok(())
            }
        }
    }

    /// Move every access unit the encoder produced since the last call.
    pub fn take_samples(&mut self, out: &mut Vec<EncodedSample>) {
        match &mut self.backend {
            #[cfg(target_os = "macos")]
            Backend::VideoToolbox(encoder) => encoder.take_samples(out),
            #[cfg(target_os = "windows")]
            Backend::MediaFoundation(encoder) => encoder.take_samples(out),
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            Backend::Unsupported => {
                let _ = out;
            }
        }
    }

    /// Whether the platform encoder reported hardware acceleration. Diagnostics
    /// only: a software encoder is still correct, just slower than real time on
    /// a large display.
    pub fn hardware_accelerated(&self) -> Option<bool> {
        match &self.backend {
            #[cfg(target_os = "macos")]
            Backend::VideoToolbox(encoder) => encoder.hardware_accelerated(),
            #[cfg(target_os = "windows")]
            Backend::MediaFoundation(_) => None,
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            Backend::Unsupported => None,
        }
    }

    /// Parameter sets of the encoded stream, available after the first sample.
    pub fn parameter_sets(&self) -> Option<ParameterSets> {
        match &self.backend {
            #[cfg(target_os = "macos")]
            Backend::VideoToolbox(encoder) => encoder.parameter_sets(),
            #[cfg(target_os = "windows")]
            Backend::MediaFoundation(encoder) => encoder.parameter_sets(),
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            Backend::Unsupported => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{FrameTimeline, supported};
    use crate::recording::TIMESCALE;

    #[test]
    fn frame_durations_follow_the_captured_timeline() {
        let mut timeline = FrameTimeline::default();
        assert_eq!(timeline.advance(Duration::ZERO), None);
        // A 60 Hz presentation is stored as an exact 1/60 s sample.
        assert_eq!(
            timeline.advance(Duration::from_nanos(16_666_667)),
            Some(1500)
        );
        // A static desktop stays one sample until the next change, two seconds
        // later, instead of being filled with repeated frames.
        assert_eq!(
            timeline.advance(Duration::from_nanos(16_666_667 + 2_000_000_000)),
            Some(TIMESCALE * 2)
        );
    }

    #[test]
    fn the_last_frame_is_closed_at_the_stop_instant() {
        let mut timeline = FrameTimeline::default();
        timeline.advance(Duration::from_millis(100));
        assert_eq!(timeline.finish(Duration::from_millis(150)), Some(4500));
        // Finishing twice (or after a flush) must not invent another sample.
        assert_eq!(timeline.finish(Duration::from_millis(200)), None);
    }

    #[test]
    fn sub_millisecond_frames_still_advance_the_timeline() {
        let mut timeline = FrameTimeline::default();
        timeline.advance(Duration::ZERO);
        assert_eq!(timeline.advance(Duration::from_micros(200)), Some(18));
        assert_eq!(
            timeline.finish(Duration::from_micros(200)),
            Some(1),
            "a zero-length sample would stall playback"
        );
    }

    #[test]
    fn recording_support_matches_the_platform_encoder() {
        assert_eq!(
            supported(),
            cfg!(any(target_os = "macos", target_os = "windows"))
        );
    }
}
