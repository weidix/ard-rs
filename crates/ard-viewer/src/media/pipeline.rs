//! Live decode pipeline: UDP/SRTP/RTP receive (ard-core) -> VideoToolbox ->
//! RGBA callback.

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ard_rs::ArdMediaStream;
use ard_rs::media_stream::UdpStreamKind;
use ard_rs::media_stream::udp::{AVC_VIDEO_SLICE_COUNT, AvcStreamCrypto, AvcVideoStreamReceiver};

use super::DecodedFrame;
use super::vt::VideoToolboxDecoder;

/// Spawn the AVC video receive/decode loop for one stream.
///
/// The loop blocks on UDP, decrypts SRTP, reassembles RTP into access units
/// (all inside ard-core), decodes with VideoToolbox and invokes `on_frame`
/// with each displayable RGBA frame. It stops when `stop` is set or the
/// remote end stays silent for `idle_timeout`.
pub fn spawn_avc_video_pipeline(
    media: ArdMediaStream,
    target_dimensions: (u32, u32),
    stop: Arc<AtomicBool>,
    mut on_frame: impl FnMut(DecodedFrame) + Send + 'static,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("ard-media-stream".into())
        .spawn(move || {
            let (endpoints, key_blob, feedback_key_blob, codec_config, remote_ssrc, local_ssrc) =
                media.into_video_pipeline_parts_with_config();
            let mut key_blob = key_blob;
            let mut feedback_key_blob = feedback_key_blob;
            let Some(codec) = codec_config.codec else {
                key_blob.fill(0);
                feedback_key_blob.fill(0);
                return;
            };
            let Some(payload_type) = codec_config.payload_type else {
                key_blob.fill(0);
                feedback_key_blob.fill(0);
                return;
            };
            let mut receiver = match AvcVideoStreamReceiver::new(
                &endpoints,
                UdpStreamKind::Video1,
                AvcStreamCrypto {
                    server_to_viewer_key_blob: &key_blob,
                    viewer_to_server_key_blob: &feedback_key_blob,
                    remote_ssrc,
                    local_ssrc,
                },
                codec,
                payload_type,
            ) {
                Ok(receiver) => receiver,
                Err(_) => {
                    key_blob.fill(0);
                    feedback_key_blob.fill(0);
                    return;
                }
            };
            // SrtpContext keeps only the derived session material. Do not
            // retain the negotiated master blob for the lifetime of the
            // receive/decode thread.
            key_blob.fill(0);
            feedback_key_blob.fill(0);
            // The four SSRCs are one serial reference chain. The receiver
            // schedules completed access units by timestamp and slice index,
            // matching AVConference's interleaved-stream scheduler.
            let mut decoder = VideoToolboxDecoder::new(codec);
            let mut compositor = SliceCompositor::new(target_dimensions);
            let mut last_frame = Instant::now();
            const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
            while !stop.load(Ordering::Relaxed) {
                match receiver.receive() {
                    Ok(Some((slice_index, unit))) => {
                        let encoded_bytes = unit.avcc_len();
                        if let Some(frame) = decoder.decode(&unit)
                            && let Some(frame) = compositor.push(slice_index, encoded_bytes, frame)
                        {
                            last_frame = Instant::now();
                            on_frame(frame);
                        }
                    }
                    Ok(None) => {}
                    Err(_) => {
                        if last_frame.elapsed() > IDLE_TIMEOUT {
                            break;
                        }
                    }
                }
            }
        })
        .expect("AVC media pipeline thread should start")
}

pub(crate) struct SliceCompositor {
    slices: [Option<DecodedFrame>; AVC_VIDEO_SLICE_COUNT],
    target_dimensions: (u32, u32),
}

impl SliceCompositor {
    pub(crate) fn new(target_dimensions: (u32, u32)) -> Self {
        Self {
            slices: std::array::from_fn(|_| None),
            target_dimensions,
        }
    }

    pub(crate) fn push(
        &mut self,
        slice_index: usize,
        encoded_bytes: usize,
        frame: DecodedFrame,
    ) -> Option<DecodedFrame> {
        let slot = self.slices.get_mut(slice_index)?;
        *slot = Some(frame);

        let width = self.slices[0].as_ref()?.width;
        let (target_width, height) = self.target_dimensions;
        if width != target_width || height == 0 {
            return None;
        }
        let rgba_len = usize::try_from(width)
            .ok()?
            .checked_mul(usize::try_from(height).ok()?)?
            .checked_mul(4)?;
        let mut rgba = Vec::with_capacity(rgba_len);
        let row_bytes = usize::try_from(width).ok()?.checked_mul(4)?;
        let mut remaining_rows = usize::try_from(height).ok()?;
        let encoded_height = self.slices.iter().try_fold(0u32, |height, slice| {
            height.checked_add(slice.as_ref()?.height)
        })?;
        for slice in &self.slices {
            let slice = slice.as_ref()?;
            if slice.width != width {
                return None;
            }
            let rows = usize::try_from(slice.height).ok()?.min(remaining_rows);
            rgba.extend_from_slice(slice.rgba.get(..rows.checked_mul(row_bytes)?)?);
            remaining_rows -= rows;
        }
        // The encoder rounds the bottom AVC band up to a macroblock boundary.
        // On Apple hosts the first retained chroma row next to that padding is
        // green as well. Preserve the advertised RFB height by extending the
        // preceding valid row over that one boundary row.
        if encoded_height > height && height > 1 && rgba.len() >= row_bytes * 2 {
            let bottom_start = rgba.len() - row_bytes;
            let (preceding, bottom) = rgba.split_at_mut(bottom_start);
            bottom.copy_from_slice(&preceding[preceding.len() - row_bytes..]);
        }
        (remaining_rows == 0 && rgba.len() == rgba_len).then_some(DecodedFrame {
            width,
            height,
            encoded_bytes,
            rgba,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slice(color: [u8; 4]) -> DecodedFrame {
        DecodedFrame {
            width: 2,
            height: 1,
            encoded_bytes: 0,
            rgba: [color, color].concat(),
        }
    }

    fn two_row_slice(top: [u8; 4], bottom: [u8; 4]) -> DecodedFrame {
        DecodedFrame {
            width: 2,
            height: 2,
            encoded_bytes: 0,
            rgba: [top, top, bottom, bottom].concat(),
        }
    }

    #[test]
    fn composites_four_native_slices_from_top_to_bottom() {
        let mut compositor = SliceCompositor::new((2, 4));
        assert!(compositor.push(2, 3, slice([2, 0, 0, 255])).is_none());
        assert!(compositor.push(0, 1, slice([0, 0, 0, 255])).is_none());
        assert!(compositor.push(3, 4, slice([3, 0, 0, 255])).is_none());
        let frame = compositor
            .push(1, 2, slice([1, 0, 0, 255]))
            .expect("all four slices compose");
        assert_eq!((frame.width, frame.height), (2, 4));
        assert_eq!(frame.encoded_bytes, 2);
        assert_eq!(
            frame
                .rgba
                .chunks_exact(8)
                .map(|row| row[0])
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
    }

    #[test]
    fn retains_unchanged_slices_for_incremental_updates() {
        let mut compositor = SliceCompositor::new((2, 4));
        for index in 0..AVC_VIDEO_SLICE_COUNT {
            let output = compositor.push(index, 1, slice([index as u8, 0, 0, 255]));
            assert_eq!(output.is_some(), index == AVC_VIDEO_SLICE_COUNT - 1);
        }
        let frame = compositor
            .push(2, 9, slice([9, 0, 0, 255]))
            .expect("one changed slice produces a complete frame");
        assert_eq!(
            frame
                .rgba
                .chunks_exact(8)
                .map(|row| row[0])
                .collect::<Vec<_>>(),
            vec![0, 1, 9, 3]
        );
    }

    #[test]
    fn crops_encoded_padding_to_the_rfb_framebuffer_height() {
        let mut compositor = SliceCompositor::new((2, 7));
        for index in 0..AVC_VIDEO_SLICE_COUNT - 1 {
            assert!(
                compositor
                    .push(
                        index,
                        1,
                        two_row_slice([index as u8, 0, 0, 255], [index as u8, 0, 0, 255],),
                    )
                    .is_none()
            );
        }
        let frame = compositor
            .push(3, 1, two_row_slice([0, 255, 0, 255], [0, 255, 0, 255]))
            .expect("the padded bottom slice completes the frame");
        assert_eq!((frame.width, frame.height), (2, 7));
        assert_eq!(
            frame
                .rgba
                .chunks_exact(8)
                .map(|row| row[0])
                .collect::<Vec<_>>(),
            vec![0, 0, 1, 1, 2, 2, 2]
        );
    }
}
