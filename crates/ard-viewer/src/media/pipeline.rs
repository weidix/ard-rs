//! Live decode pipeline: UDP/SRTP/RTP receive (ard-core) -> platform decoder
//! (VideoToolbox or MFT) -> native NV12 callback.

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ard_rs::ArdMediaStream;
use ard_rs::media_stream::UdpStreamKind;
use ard_rs::media_stream::udp::{AVC_VIDEO_SLICE_COUNT, AvcStreamCrypto, AvcVideoStreamReceiver};

#[cfg(target_os = "windows")]
use super::mft::MftDecoder as PlatformVideoDecoder;
#[cfg(target_os = "macos")]
use super::vt::VideoToolboxDecoder as PlatformVideoDecoder;
use super::{DecodedFrame, DecodedOutput, DecodedSlice, DecodedSliceUpdate, YuvMatrix, YuvRange};

/// Spawn the AVC video receive/decode loop for one stream.
///
/// The loop blocks on UDP, decrypts SRTP, reassembles RTP into access units
/// (all inside ard-core), decodes with the platform backend and invokes `on_frame`
/// with each displayable NV12 frame. It stops when `stop` is set or the
/// remote end stays silent for `idle_timeout`.
pub fn spawn_avc_video_pipeline(
    media: ArdMediaStream,
    target_dimensions: (u32, u32),
    stop: Arc<AtomicBool>,
    mut on_frame: impl FnMut(Result<DecodedFrame, String>) + Send + 'static,
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
                on_frame(Err("媒体协商未选择 H.264/HEVC 编解码器".into()));
                return;
            };
            let Some(payload_type) = codec_config.payload_type else {
                key_blob.fill(0);
                feedback_key_blob.fill(0);
                on_frame(Err("媒体协商未返回 RTP 负载类型".into()));
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
                Err(error) => {
                    key_blob.fill(0);
                    feedback_key_blob.fill(0);
                    on_frame(Err(format!("AVC UDP/SRTP 接收器初始化失败：{error}")));
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
            let mut decoder = PlatformVideoDecoder::new(codec);
            let mut compositor = SliceCompositor::new(target_dimensions);
            let mut observed_packet_losses = receiver.packet_losses();
            let mut last_frame = Instant::now();
            const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
            while !stop.load(Ordering::Relaxed) {
                let received = receiver.receive_frame();
                if receiver.packet_losses() != observed_packet_losses {
                    observed_packet_losses = receiver.packet_losses();
                    decoder.require_sync();
                    compositor.reset();
                }
                match received {
                    Ok(Some(batch)) => {
                        let timestamp = batch.timestamp;
                        let mut outputs = Vec::with_capacity(batch.access_units.len());
                        let mut failure = None;
                        for (slice_index, unit) in batch.access_units {
                            outputs.extend(decoder.decode(slice_index, &unit));
                            if let Some(error) = decoder.take_errors().into_iter().next() {
                                failure = Some(error);
                                break;
                            }
                        }
                        if failure.is_none() {
                            outputs.extend(decoder.finish_frame());
                            failure = decoder.take_errors().into_iter().next();
                        }
                        if failure.is_none() {
                            failure =
                                apply_decoder_outputs(timestamp, outputs, &mut compositor).err();
                        }
                        if failure.is_none() {
                            match compositor.finish_frame() {
                                Ok(Some(frame)) => {
                                    last_frame = Instant::now();
                                    on_frame(Ok(frame));
                                }
                                Ok(None) => {}
                                Err(error) => failure = Some(error),
                            }
                        }
                        if let Some(error) = failure {
                            decoder.require_sync();
                            compositor.reset();
                            if let Err(feedback_error) = receiver.request_keyframe() {
                                on_frame(Err(format!("{error}；关键帧请求失败：{feedback_error}")));
                            } else {
                                on_frame(Err(error));
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        decoder.require_sync();
                        compositor.reset();
                        on_frame(Err(format!("AVC RTP/SRTP 接收失败：{error}")));
                        if last_frame.elapsed() > IDLE_TIMEOUT {
                            break;
                        }
                    }
                }
            }
            let outputs = decoder.flush();
            for error in decoder.take_errors() {
                on_frame(Err(error));
            }
            if !outputs.is_empty() {
                on_frame(Err(format!(
                    "视频解码器停止时仍返回 {} 个未归属 RTP 帧批次的输出",
                    outputs.len()
                )));
            }
        })
        .expect("AVC media pipeline thread should start")
}

fn apply_decoder_outputs(
    timestamp: u32,
    outputs: Vec<DecodedOutput>,
    compositor: &mut SliceCompositor,
) -> Result<(), String> {
    for output in outputs {
        if output.timestamp != timestamp {
            return Err(format!(
                "视频解码输出跨越 RTP 帧边界：expected={timestamp} actual={}",
                output.timestamp
            ));
        }
        if let Some(error) = output.conversion_error {
            return Err(error);
        }
        if output.status != 0 {
            return Err(format!(
                "视频解码失败：status={} flags={:#x}",
                output.status, output.info_flags
            ));
        }
        compositor.push(output.stream_index, output.encoded_bytes, output.frame)?;
    }
    Ok(())
}

pub(crate) struct SliceCompositor {
    metadata: [Option<SliceMetadata>; AVC_VIDEO_SLICE_COUNT],
    pending: [Option<DecodedSlice>; AVC_VIDEO_SLICE_COUNT],
    target_dimensions: (u32, u32),
    pending_encoded_bytes: usize,
    dirty: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SliceMetadata {
    width: u32,
    height: u32,
    range: YuvRange,
    matrix: YuvMatrix,
}

impl From<&DecodedSlice> for SliceMetadata {
    fn from(frame: &DecodedSlice) -> Self {
        Self {
            width: frame.width,
            height: frame.height,
            range: frame.range,
            matrix: frame.matrix,
        }
    }
}

impl SliceCompositor {
    pub(crate) fn new(target_dimensions: (u32, u32)) -> Self {
        Self {
            metadata: std::array::from_fn(|_| None),
            pending: std::array::from_fn(|_| None),
            target_dimensions,
            pending_encoded_bytes: 0,
            dirty: false,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.metadata = std::array::from_fn(|_| None);
        self.pending = std::array::from_fn(|_| None);
        self.pending_encoded_bytes = 0;
        self.dirty = false;
    }

    pub(crate) fn push(
        &mut self,
        slice_index: usize,
        encoded_bytes: usize,
        frame: Option<DecodedSlice>,
    ) -> Result<(), String> {
        let mut layout_reset = false;
        if let Some(frame) = frame {
            let metadata = SliceMetadata::from(&frame);
            let slot = self
                .metadata
                .get(slice_index)
                .ok_or_else(|| format!("无效的 AVC 分片索引：{slice_index}"))?;
            if slot.is_some_and(|current| current != metadata) {
                self.reset();
                layout_reset = true;
            }
            self.metadata[slice_index] = Some(metadata);
            self.pending[slice_index] = Some(frame);
            self.dirty = true;
        }
        self.pending_encoded_bytes = if layout_reset {
            encoded_bytes
        } else {
            self.pending_encoded_bytes.saturating_add(encoded_bytes)
        };
        Ok(())
    }

    pub(crate) fn finish_frame(&mut self) -> Result<Option<DecodedFrame>, String> {
        if !self.dirty {
            self.pending_encoded_bytes = 0;
            return Ok(None);
        }

        let Some(first) = self.metadata[0] else {
            return Ok(None);
        };
        let width = first.width;
        let (target_width, height) = self.target_dimensions;
        if width != target_width || height == 0 {
            return Err(format!(
                "AVC 原生分片尺寸与远端帧缓冲不一致：slice_width={width} framebuffer={target_width}x{height}"
            ));
        }
        let mut remaining_rows = height;
        let mut remaining_uv_rows = height.div_ceil(2);
        let mut y_origin = 0_u32;
        let mut uv_origin = 0_u32;
        let mut layouts = [(0, 0, 0, 0); AVC_VIDEO_SLICE_COUNT];
        let range = first.range;
        let matrix = first.matrix;
        for (index, metadata) in self.metadata.iter().enumerate() {
            let Some(metadata) = metadata.as_ref() else {
                return Ok(None);
            };
            if metadata.width != width || metadata.range != range || metadata.matrix != matrix {
                return Err(format!("AVC 分片 {index} 的 NV12 布局或色彩矩阵不一致"));
            }
            let rows = metadata.height.min(remaining_rows);
            let uv_rows = rows.div_ceil(2).min(remaining_uv_rows);
            layouts[index] = (y_origin, rows, uv_origin, uv_rows);
            y_origin = y_origin
                .checked_add(rows)
                .ok_or_else(|| "AVC 亮度分片位置溢出".to_owned())?;
            uv_origin = uv_origin
                .checked_add(uv_rows)
                .ok_or_else(|| "AVC 色度分片位置溢出".to_owned())?;
            remaining_rows -= rows;
            remaining_uv_rows -= uv_rows;
        }
        if remaining_rows != 0 || remaining_uv_rows != 0 {
            return Err(format!(
                "AVC 四分片未覆盖远端帧缓冲：missing_y={remaining_rows} missing_uv={remaining_uv_rows}"
            ));
        }
        let updates = self
            .pending
            .iter_mut()
            .enumerate()
            .filter_map(|(slice_index, pending)| {
                let pixels = pending.take()?;
                let (y_origin, y_rows, uv_origin, uv_rows) = layouts[slice_index];
                Some(DecodedSliceUpdate {
                    slice_index,
                    y_origin,
                    y_rows,
                    uv_origin,
                    uv_rows,
                    pixels,
                })
            })
            .collect::<Vec<_>>();
        self.dirty = false;
        Ok(Some(DecodedFrame {
            width,
            height,
            encoded_bytes: std::mem::take(&mut self.pending_encoded_bytes),
            range,
            matrix,
            updates,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slice(luma: u8) -> DecodedSlice {
        DecodedSlice {
            width: 2,
            height: 1,
            y_plane: vec![luma; 2],
            uv_plane: vec![128; 2],
            range: super::super::YuvRange::Video,
            matrix: super::super::YuvMatrix::Bt709,
        }
    }

    fn two_row_slice(top: u8, bottom: u8) -> DecodedSlice {
        DecodedSlice {
            width: 2,
            height: 2,
            y_plane: vec![top, top, bottom, bottom],
            uv_plane: vec![128; 2],
            range: super::super::YuvRange::Video,
            matrix: super::super::YuvMatrix::Bt709,
        }
    }

    #[test]
    fn batches_four_native_slices_with_gpu_destinations() {
        let mut compositor = SliceCompositor::new((2, 4));
        compositor.push(2, 3, Some(slice(2))).expect("slice 2");
        compositor.push(0, 1, Some(slice(0))).expect("slice 0");
        compositor.push(3, 4, Some(slice(3))).expect("slice 3");
        compositor.push(1, 2, Some(slice(1))).expect("slice 1");
        let frame = compositor
            .finish_frame()
            .expect("valid native layout")
            .expect("all four slices compose");
        assert_eq!((frame.width, frame.height), (2, 4));
        assert_eq!(frame.encoded_bytes, 10);
        assert_eq!(frame.updates.len(), 4);
        assert_eq!(
            frame
                .updates
                .iter()
                .map(|update| (update.slice_index, update.y_origin, update.y_rows))
                .collect::<Vec<_>>(),
            vec![(0, 0, 1), (1, 1, 1), (2, 2, 1), (3, 3, 1)]
        );
    }

    #[test]
    fn retains_unchanged_slices_for_incremental_updates() {
        let mut compositor = SliceCompositor::new((2, 4));
        for index in 0..AVC_VIDEO_SLICE_COUNT {
            compositor
                .push(index, 1, Some(slice(index as u8)))
                .expect("initial slice");
        }
        compositor
            .finish_frame()
            .expect("valid initial frame")
            .expect("initial frame");
        compositor
            .push(2, 9, Some(slice(9)))
            .expect("changed slice");
        let frame = compositor
            .finish_frame()
            .expect("valid sparse frame")
            .expect("one changed slice produces a complete frame");
        assert_eq!(frame.updates.len(), 1);
        assert_eq!(frame.updates[0].slice_index, 2);
        assert_eq!(frame.updates[0].y_origin, 2);
        assert_eq!(frame.updates[0].pixels.y_plane, vec![9, 9]);
    }

    #[test]
    fn crops_encoded_padding_to_the_rfb_framebuffer_height() {
        let mut compositor = SliceCompositor::new((2, 7));
        for index in 0..AVC_VIDEO_SLICE_COUNT - 1 {
            compositor
                .push(index, 1, Some(two_row_slice(index as u8, index as u8)))
                .expect("padded slice");
        }
        compositor
            .push(3, 1, Some(two_row_slice(3, 255)))
            .expect("bottom padded slice");
        let frame = compositor
            .finish_frame()
            .expect("valid cropped frame")
            .expect("the padded bottom slice completes the frame");
        assert_eq!((frame.width, frame.height), (2, 7));
        assert_eq!(frame.updates[3].y_origin, 6);
        assert_eq!(frame.updates[3].y_rows, 1);
        assert_eq!(frame.updates[3].uv_origin, 3);
        assert_eq!(frame.updates[3].uv_rows, 1);
    }
}
