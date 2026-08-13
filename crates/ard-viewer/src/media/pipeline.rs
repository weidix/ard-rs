//! Live decode pipeline: UDP/SRTP/RTP receive (ard-core) -> platform decoder
//! (VideoToolbox or MFT) -> native NV12 callback.

#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ard_rs::ArdMediaStream;
use ard_rs::media_stream::UdpStreamKind;
use ard_rs::media_stream::udp::{
    AVC_VIDEO_SLICE_COUNT, AvcFrameBatch, AvcStreamCrypto, AvcVideoStreamReceiver,
};

#[cfg(target_os = "windows")]
use super::mft::MftDecoder as PlatformVideoDecoder;
#[cfg(target_os = "macos")]
use super::vt::VideoToolboxDecoder as PlatformVideoDecoder;
use super::{
    AvcFrameTiming, DecodedFrame, DecodedOutput, DecodedSlice, DecodedSliceUpdate, YuvMatrix,
    YuvRange,
};

/// Allows VideoToolbox/MFT one bounded startup window to create its hardware
/// session while UDP continues draining. At 60 Hz this is at most 267 ms;
/// steady-state queue delay is measured separately and any further growth
/// resets the prediction chain instead of accumulating seconds of latency.
const AVC_RECEIVE_QUEUE_CAPACITY: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AvcReceiveResetReason {
    PacketLoss,
    ConsumerOverrun,
    DecoderRequested,
    ReceiverError,
}

#[derive(Debug)]
pub(crate) enum AvcReceiveEvent {
    Frame(AvcFrameBatch),
    Reset(AvcReceiveResetReason),
    Error(String),
}

#[derive(Debug, Default)]
struct AvcReceiveQueueState {
    events: VecDeque<AvcReceiveEvent>,
    closed: bool,
}

#[derive(Debug, Default)]
struct AvcReceiveQueue {
    state: Mutex<AvcReceiveQueueState>,
    available: Condvar,
}

impl AvcReceiveQueue {
    fn push_frame(&self, batch: AvcFrameBatch) -> bool {
        let mut state = self.state.lock().expect("AVC receive queue poisoned");
        if state.events.len() >= AVC_RECEIVE_QUEUE_CAPACITY {
            state.events.clear();
            state.events.push_back(AvcReceiveEvent::Reset(
                AvcReceiveResetReason::ConsumerOverrun,
            ));
            self.available.notify_one();
            return false;
        }
        state.events.push_back(AvcReceiveEvent::Frame(batch));
        self.available.notify_one();
        true
    }

    fn reset(&self, reason: AvcReceiveResetReason) {
        let mut state = self.state.lock().expect("AVC receive queue poisoned");
        state.events.clear();
        state.events.push_back(AvcReceiveEvent::Reset(reason));
        self.available.notify_one();
    }

    fn push_error(&self, error: String) {
        let mut state = self.state.lock().expect("AVC receive queue poisoned");
        if state.events.len() >= AVC_RECEIVE_QUEUE_CAPACITY {
            state.events.pop_front();
        }
        state.events.push_back(AvcReceiveEvent::Error(error));
        self.available.notify_one();
    }

    fn pop_timeout(&self, timeout: Duration) -> Option<AvcReceiveEvent> {
        let mut state = self.state.lock().expect("AVC receive queue poisoned");
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(event) = state.events.pop_front() {
                return Some(event);
            }
            if state.closed {
                return None;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let (next, wait) = self
                .available
                .wait_timeout(state, remaining)
                .expect("AVC receive queue poisoned while waiting");
            state = next;
            if wait.timed_out() && state.events.is_empty() {
                return None;
            }
        }
    }

    fn close(&self) {
        let mut state = self.state.lock().expect("AVC receive queue poisoned");
        state.closed = true;
        self.available.notify_all();
    }
}

#[derive(Debug, Default)]
struct AvcReceiveAtomicStats {
    packets_received: AtomicUsize,
    decrypted_packets: AtomicUsize,
    heartbeats_sent: AtomicUsize,
    packet_losses: AtomicUsize,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct AvcReceiveStats {
    pub packets_received: usize,
    pub decrypted_packets: usize,
    pub heartbeats_sent: usize,
    pub packet_losses: usize,
}

impl AvcReceiveAtomicStats {
    fn update(&self, receiver: &AvcVideoStreamReceiver) {
        self.packets_received
            .store(receiver.packets_received(), Ordering::Relaxed);
        self.decrypted_packets
            .store(receiver.decrypted_packets(), Ordering::Relaxed);
        self.heartbeats_sent
            .store(receiver.heartbeats_sent(), Ordering::Relaxed);
        self.packet_losses
            .store(receiver.packet_losses(), Ordering::Relaxed);
    }

    fn snapshot(&self) -> AvcReceiveStats {
        AvcReceiveStats {
            packets_received: self.packets_received.load(Ordering::Relaxed),
            decrypted_packets: self.decrypted_packets.load(Ordering::Relaxed),
            heartbeats_sent: self.heartbeats_sent.load(Ordering::Relaxed),
            packet_losses: self.packet_losses.load(Ordering::Relaxed),
        }
    }
}

/// Dedicated UDP/SRTP receive pump. The platform decoder never owns the UDP
/// read loop, so a compressed-frame burst continues to drain while
/// VideoToolbox/MFT and the renderer consume the preceding frame. Overflow is
/// prediction-chain loss: queued dependent frames are cleared and the server
/// is asked for a real sync frame instead of silently dropping/coalescing one.
pub(crate) struct AvcReceivePump {
    queue: Arc<AvcReceiveQueue>,
    commands: mpsc::Sender<()>,
    internal_stop: Arc<AtomicBool>,
    stats: Arc<AvcReceiveAtomicStats>,
    join: Option<JoinHandle<()>>,
}

impl AvcReceivePump {
    pub(crate) fn spawn(
        mut receiver: AvcVideoStreamReceiver,
        external_stop: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        let queue = Arc::new(AvcReceiveQueue::default());
        let internal_stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(AvcReceiveAtomicStats::default());
        let (commands, command_receiver) = mpsc::channel();
        let thread_queue = Arc::clone(&queue);
        let thread_stop = Arc::clone(&internal_stop);
        let thread_stats = Arc::clone(&stats);
        let join = thread::Builder::new()
            .name("ard-media-receive".into())
            .spawn(move || {
                let mut observed_packet_losses = receiver.packet_losses();
                while !external_stop.load(Ordering::Relaxed)
                    && !thread_stop.load(Ordering::Relaxed)
                {
                    let mut decoder_requested = command_receiver.try_recv().is_ok();
                    while command_receiver.try_recv().is_ok() {
                        decoder_requested = true;
                    }
                    if decoder_requested {
                        thread_queue.reset(AvcReceiveResetReason::DecoderRequested);
                        if let Err(error) = receiver.request_keyframe() {
                            thread_queue.push_error(format!("关键帧请求失败：{error}"));
                        }
                    }

                    let received = receiver.receive_frame();
                    thread_stats.update(&receiver);
                    if receiver.packet_losses() != observed_packet_losses {
                        observed_packet_losses = receiver.packet_losses();
                        thread_queue.reset(AvcReceiveResetReason::PacketLoss);
                    }

                    // A decoder reset may have raced with the blocking UDP
                    // read. Process it before publishing that read's batch so
                    // no old dependent frame crosses the reset boundary.
                    let mut decoder_requested = command_receiver.try_recv().is_ok();
                    while command_receiver.try_recv().is_ok() {
                        decoder_requested = true;
                    }
                    if decoder_requested {
                        thread_queue.reset(AvcReceiveResetReason::DecoderRequested);
                        if let Err(error) = receiver.request_keyframe() {
                            thread_queue.push_error(format!("关键帧请求失败：{error}"));
                        }
                        continue;
                    }

                    match received {
                        Ok(Some(batch)) => {
                            if !thread_queue.push_frame(batch)
                                && let Err(error) = receiver.request_keyframe()
                            {
                                thread_queue.push_error(format!(
                                    "视频接收队列过载且关键帧请求失败：{error}"
                                ));
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            thread_queue.reset(AvcReceiveResetReason::ReceiverError);
                            if let Err(feedback_error) = receiver.request_keyframe() {
                                thread_queue.push_error(format!(
                                    "AVC RTP/SRTP 接收失败：{error}；关键帧请求失败：{feedback_error}"
                                ));
                            } else {
                                thread_queue
                                    .push_error(format!("AVC RTP/SRTP 接收失败：{error}"));
                            }
                        }
                    }
                }
                thread_stats.update(&receiver);
                thread_queue.close();
            })
            .map_err(|error| format!("无法启动 AVC 网络接收线程：{error}"))?;
        Ok(Self {
            queue,
            commands,
            internal_stop,
            stats,
            join: Some(join),
        })
    }

    pub(crate) fn receive_timeout(&self, timeout: Duration) -> Option<AvcReceiveEvent> {
        self.queue.pop_timeout(timeout)
    }

    pub(crate) fn request_keyframe(&self) -> Result<(), String> {
        self.queue.reset(AvcReceiveResetReason::DecoderRequested);
        self.commands
            .send(())
            .map_err(|_| "AVC 网络接收线程已经停止".to_owned())
    }

    pub(crate) fn stats(&self) -> AvcReceiveStats {
        self.stats.snapshot()
    }
}

impl Drop for AvcReceivePump {
    fn drop(&mut self) {
        self.internal_stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

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
            let negotiated_dimensions = codec_config.width.zip(codec_config.height);
            let receiver = match AvcVideoStreamReceiver::new(
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
            let receive_pump = match AvcReceivePump::spawn(receiver, Arc::clone(&stop)) {
                Ok(pump) => pump,
                Err(error) => {
                    on_frame(Err(error));
                    return;
                }
            };
            // The four SSRCs are one serial reference chain. The dedicated
            // receive pump drains UDP continuously and schedules access units
            // by the native global DON/DONL order.
            let mut decoder = PlatformVideoDecoder::new(codec);
            if let Some(error) = decoder.take_errors().into_iter().next() {
                on_frame(Err(error));
                return;
            }
            let mut output_assembler =
                DecoderOutputAssembler::new(target_dimensions, negotiated_dimensions);
            let mut last_frame = Instant::now();
            const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
            while !stop.load(Ordering::Relaxed) {
                match receive_pump.receive_timeout(Duration::from_millis(20)) {
                    Some(AvcReceiveEvent::Frame(batch)) => {
                        let timestamp = batch.timestamp;
                        let first_packet_received_at = batch.first_packet_received_at;
                        let first_access_unit_completed_at = batch.first_access_unit_completed_at;
                        let batch_released_at = batch.released_at;
                        let access_unit_count = batch.access_units.len();
                        let mut outputs = Vec::with_capacity(batch.access_units.len());
                        let mut failure = output_assembler
                            .register_batch(
                                timestamp,
                                access_unit_count,
                                PendingFrameTiming {
                                    first_packet_received_at,
                                    first_access_unit_completed_at,
                                    batch_released_at,
                                },
                            )
                            .err();
                        for (slice_index, unit) in batch.access_units {
                            if failure.is_some() {
                                break;
                            }
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
                            match output_assembler.push(outputs) {
                                Ok(frames) => {
                                    for frame in frames {
                                        last_frame = Instant::now();
                                        on_frame(Ok(frame));
                                    }
                                }
                                Err(error) => failure = Some(error),
                            }
                        }
                        if let Some(error) = failure {
                            decoder.require_sync();
                            output_assembler.reset();
                            if let Err(feedback_error) = receive_pump.request_keyframe() {
                                on_frame(Err(format!("{error}；关键帧请求失败：{feedback_error}")));
                            } else {
                                on_frame(Err(error));
                            }
                        }
                    }
                    Some(AvcReceiveEvent::Reset(reason)) => {
                        decoder.require_sync();
                        output_assembler.reset();
                        if reason == AvcReceiveResetReason::ConsumerOverrun {
                            on_frame(Err(
                                "视频解码消费速度落后于网络接收，预测链已重置并请求真实关键帧"
                                    .into(),
                            ));
                        }
                    }
                    Some(AvcReceiveEvent::Error(error)) => {
                        decoder.require_sync();
                        output_assembler.reset();
                        on_frame(Err(error));
                        if last_frame.elapsed() > IDLE_TIMEOUT {
                            break;
                        }
                    }
                    None if last_frame.elapsed() > IDLE_TIMEOUT => break,
                    None => {}
                }
            }
            drop(receive_pump);
            let outputs = decoder.flush();
            for error in decoder.take_errors() {
                on_frame(Err(error));
            }
            match output_assembler.push(outputs) {
                Ok(frames) => {
                    for frame in frames {
                        on_frame(Ok(frame));
                    }
                }
                Err(error) => on_frame(Err(error)),
            }
        })
        .expect("AVC media pipeline thread should start")
}

#[derive(Debug, Clone, Copy)]
struct PendingFrameTiming {
    first_packet_received_at: Instant,
    first_access_unit_completed_at: Instant,
    batch_released_at: Instant,
}

#[derive(Debug, Clone, Copy)]
struct PendingDecodeBatch {
    expected_outputs: usize,
    timing: PendingFrameTiming,
}

struct DecoderOutputAssembler {
    compositor: SliceCompositor,
    pending: HashMap<u32, PendingDecodeBatch>,
    active_timestamp: Option<u32>,
    active_outputs: usize,
    negotiated_dimensions: Option<(u32, u32)>,
}

impl DecoderOutputAssembler {
    fn new(target_dimensions: (u32, u32), negotiated_dimensions: Option<(u32, u32)>) -> Self {
        Self {
            compositor: SliceCompositor::new(target_dimensions),
            pending: HashMap::new(),
            active_timestamp: None,
            active_outputs: 0,
            negotiated_dimensions,
        }
    }

    fn register_batch(
        &mut self,
        timestamp: u32,
        expected_outputs: usize,
        timing: PendingFrameTiming,
    ) -> Result<(), String> {
        if expected_outputs == 0 {
            return Err(format!("AVC 时间戳 {timestamp} 没有 access unit"));
        }
        if self.pending.len() >= AVC_RECEIVE_QUEUE_CAPACITY * 4 {
            return Err("视频解码器延迟超过允许的 RTP 批次数".into());
        }
        if self
            .pending
            .insert(
                timestamp,
                PendingDecodeBatch {
                    expected_outputs,
                    timing,
                },
            )
            .is_some()
        {
            return Err(format!("重复提交 AVC RTP 时间戳 {timestamp}"));
        }
        Ok(())
    }

    fn push(&mut self, outputs: Vec<DecodedOutput>) -> Result<Vec<DecodedFrame>, String> {
        let mut frames = Vec::new();
        for output in outputs {
            if let Some(error) = output.conversion_error {
                return Err(error);
            }
            if output.status != 0 {
                return Err(format!(
                    "视频解码失败：status={} flags={:#x}",
                    output.status, output.info_flags
                ));
            }
            let batch = self
                .pending
                .get(&output.timestamp)
                .copied()
                .ok_or_else(|| {
                    format!(
                        "视频解码输出无法对应到已提交的 RTP 批次：timestamp={}",
                        output.timestamp
                    )
                })?;
            if self
                .active_timestamp
                .is_some_and(|timestamp| timestamp != output.timestamp)
            {
                return Err(format!(
                    "视频解码输出在上一 RTP 批次完成前改变时间戳：previous={} previous_outputs={} expected={} actual={}",
                    self.active_timestamp.expect("checked active timestamp"),
                    self.active_outputs,
                    self.pending
                        .get(&self.active_timestamp.expect("checked active timestamp"))
                        .map_or(0, |pending| pending.expected_outputs),
                    output.timestamp,
                ));
            }
            self.active_timestamp = Some(output.timestamp);
            self.compositor
                .push(output.stream_index, output.encoded_bytes, output.frame)?;
            self.active_outputs += 1;
            if self.active_outputs > batch.expected_outputs {
                return Err(format!(
                    "视频解码输出数量超过 RTP 批次 access unit 数：timestamp={} outputs={} expected={}",
                    output.timestamp, self.active_outputs, batch.expected_outputs
                ));
            }
            if self.active_outputs == batch.expected_outputs {
                let mut frame = self.compositor.finish_frame()?.ok_or_else(|| {
                    format!(
                        "视频解码 RTP 批次没有可显示图像：timestamp={}",
                        output.timestamp
                    )
                })?;
                let pending = self
                    .pending
                    .remove(&output.timestamp)
                    .expect("decoded batch was validated");
                frame.timing = Some(AvcFrameTiming {
                    first_packet_received_at: pending.timing.first_packet_received_at,
                    first_access_unit_completed_at: pending.timing.first_access_unit_completed_at,
                    batch_released_at: pending.timing.batch_released_at,
                    decoded_at: Instant::now(),
                    negotiated_dimensions: self.negotiated_dimensions,
                });
                frames.push(frame);
                self.active_timestamp = None;
                self.active_outputs = 0;
            }
        }
        Ok(frames)
    }

    fn reset(&mut self) {
        self.compositor.reset();
        self.pending.clear();
        self.active_timestamp = None;
        self.active_outputs = 0;
    }
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
        let (target_width, target_height) = self.target_dimensions;
        if target_width == 0
            || target_height == 0
            || !target_width.is_multiple_of(2)
            || !target_height.is_multiple_of(2)
        {
            return Err(format!(
                "AVC 目标帧缓冲尺寸无效：{target_width}x{target_height}"
            ));
        }
        for (index, metadata) in self.metadata.iter().enumerate() {
            let Some(metadata) = metadata.as_ref() else {
                return Ok(None);
            };
            if metadata.width != width
                || metadata.height != first.height
                || metadata.range != first.range
                || metadata.matrix != first.matrix
            {
                return Err(format!("AVC 分片 {index} 的 NV12 布局或色彩矩阵不一致"));
            }
        }

        let encoded_slice_height = first.height;
        let minimum_slice_height = target_height.div_ceil(AVC_VIDEO_SLICE_COUNT as u32);
        let aligned_slice_height = minimum_slice_height
            .checked_add(15)
            .map(|height| height & !15)
            .ok_or_else(|| "AVC 分片对齐高度溢出".to_owned())?;
        // Screen Sharing divides the desktop into four ordered horizontal
        // SSRC bands. The first three bands occupy the codec-aligned height;
        // only the fourth band may contain bottom padding. This is observable
        // in the native stream (for example 2224 = 560*3 + 544 and
        // 1800 = 464*3 + 408). A decoder-provided clean aperture may instead
        // make all four bands exactly one quarter high. Accept those two
        // protocol layouts and reject every arbitrary crop/scale mismatch.
        let clean_aperture_layout = target_height.is_multiple_of(AVC_VIDEO_SLICE_COUNT as u32)
            && encoded_slice_height == target_height / AVC_VIDEO_SLICE_COUNT as u32;
        let codec_aligned_layout = encoded_slice_height == aligned_slice_height;
        let leading_height = encoded_slice_height
            .checked_mul((AVC_VIDEO_SLICE_COUNT - 1) as u32)
            .ok_or_else(|| "AVC 分片总高度溢出".to_owned())?;
        let last_visible_height = target_height.checked_sub(leading_height);
        if width != target_width
            || (!clean_aperture_layout
                && (!codec_aligned_layout
                    || last_visible_height
                        .is_none_or(|height| height == 0 || height > encoded_slice_height)))
        {
            let decoded_height = encoded_slice_height.saturating_mul(AVC_VIDEO_SLICE_COUNT as u32);
            return Err(format!(
                "服务器 AVC 分片几何与请求分辨率不一致：requested={target_width}x{target_height} decoded_slices={width}x{encoded_slice_height}x{AVC_VIDEO_SLICE_COUNT} decoded_total={width}x{decoded_height} expected_aligned_slice_height={aligned_slice_height}；拒绝任意裁切或缩放伪装成功"
            ));
        }

        let mut remaining_uv_rows = target_height.div_ceil(2);
        let mut remaining_y_rows = target_height;
        let mut y_origin = 0_u32;
        let mut uv_origin = 0_u32;
        let mut layouts = [(0, 0, 0, 0); AVC_VIDEO_SLICE_COUNT];
        let range = first.range;
        let matrix = first.matrix;
        for (index, metadata) in self.metadata.iter().enumerate() {
            let metadata = metadata.as_ref().expect("all slice metadata validated");
            let rows = metadata.height.min(remaining_y_rows);
            let uv_rows = rows.div_ceil(2).min(remaining_uv_rows);
            layouts[index] = (y_origin, rows, uv_origin, uv_rows);
            y_origin = y_origin
                .checked_add(rows)
                .ok_or_else(|| "AVC 亮度分片位置溢出".to_owned())?;
            uv_origin = uv_origin
                .checked_add(uv_rows)
                .ok_or_else(|| "AVC 色度分片位置溢出".to_owned())?;
            remaining_y_rows -= rows;
            remaining_uv_rows -= uv_rows;
        }
        if remaining_y_rows != 0 || remaining_uv_rows != 0 {
            return Err(format!(
                "AVC 四分片未覆盖远端帧缓冲：missing_y={remaining_y_rows} missing_uv={remaining_uv_rows}"
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
            height: target_height,
            encoded_bytes: std::mem::take(&mut self.pending_encoded_bytes),
            range,
            matrix,
            updates,
            timing: None,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ard_rs::media_stream::AccessUnit;

    fn compressed_batch(timestamp: u32) -> AvcFrameBatch {
        let now = Instant::now();
        AvcFrameBatch {
            timestamp,
            access_units: vec![(
                0,
                AccessUnit {
                    timestamp,
                    decode_order_number: Some(timestamp as u16),
                    nal_units: vec![vec![0x41]],
                },
            )],
            first_packet_received_at: now,
            first_access_unit_completed_at: now,
            released_at: now,
        }
    }

    #[test]
    fn receive_queue_overrun_discards_the_dependent_chain_and_emits_reset() {
        let queue = AvcReceiveQueue::default();
        for timestamp in 0..AVC_RECEIVE_QUEUE_CAPACITY as u32 {
            assert!(queue.push_frame(compressed_batch(timestamp)));
        }
        assert!(!queue.push_frame(compressed_batch(999)));
        assert!(matches!(
            queue.pop_timeout(Duration::ZERO),
            Some(AvcReceiveEvent::Reset(
                AvcReceiveResetReason::ConsumerOverrun
            ))
        ));
        assert!(queue.pop_timeout(Duration::ZERO).is_none());
    }

    #[test]
    fn receive_queue_reset_cannot_leak_an_older_prediction_batch() {
        let queue = AvcReceiveQueue::default();
        assert!(queue.push_frame(compressed_batch(1)));
        queue.reset(AvcReceiveResetReason::DecoderRequested);
        assert!(matches!(
            queue.pop_timeout(Duration::ZERO),
            Some(AvcReceiveEvent::Reset(
                AvcReceiveResetReason::DecoderRequested
            ))
        ));
        assert!(queue.pop_timeout(Duration::ZERO).is_none());
    }

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

    fn decoded_output(timestamp: u32, slice_index: usize) -> DecodedOutput {
        DecodedOutput {
            stream_index: slice_index,
            timestamp,
            submission: u64::from(timestamp) + slice_index as u64,
            encoded_bytes: 1,
            status: 0,
            info_flags: 0,
            conversion_error: None,
            frame: Some(slice(slice_index as u8)),
        }
    }

    #[test]
    fn assembles_delayed_decoder_outputs_by_their_source_timestamp() {
        let now = Instant::now();
        let timing = PendingFrameTiming {
            first_packet_received_at: now,
            first_access_unit_completed_at: now,
            batch_released_at: now,
        };
        let mut assembler = DecoderOutputAssembler::new((2, 4), Some((2, 4)));
        assembler.register_batch(10, 4, timing).expect("batch 10");
        assembler.register_batch(20, 4, timing).expect("batch 20");

        assert!(
            assembler
                .push(vec![decoded_output(10, 0), decoded_output(10, 1)])
                .expect("partial delayed output")
                .is_empty()
        );
        let first = assembler
            .push(vec![decoded_output(10, 2), decoded_output(10, 3)])
            .expect("complete delayed output");
        assert_eq!(first.len(), 1);
        assert_eq!(
            first[0]
                .updates
                .iter()
                .map(|update| update.slice_index)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );

        let second = assembler
            .push((0..4).map(|index| decoded_output(20, index)).collect())
            .expect("following delayed output");
        assert_eq!(second.len(), 1);
        assert_eq!(
            second[0]
                .timing
                .expect("frame timing")
                .negotiated_dimensions,
            Some((2, 4))
        );
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
    fn rejects_same_width_height_mismatch_instead_of_cropping() {
        let mut compositor = SliceCompositor::new((2, 7));
        for index in 0..AVC_VIDEO_SLICE_COUNT - 1 {
            compositor
                .push(index, 1, Some(two_row_slice(index as u8, index as u8)))
                .expect("padded slice");
        }
        compositor
            .push(3, 1, Some(two_row_slice(3, 255)))
            .expect("bottom padded slice");
        let error = compositor
            .finish_frame()
            .expect_err("decoded height must not be silently cropped");
        assert!(error.contains("AVC 目标帧缓冲尺寸无效：2x7"), "{error}");
    }

    #[test]
    fn removes_only_proven_bottom_padding_from_the_last_native_slice() {
        fn padded_slice(index: usize) -> DecodedSlice {
            let mut frame = DecodedSlice {
                width: 2,
                height: 16,
                y_plane: vec![index as u8; 32],
                uv_plane: vec![128; 16],
                range: YuvRange::Video,
                matrix: YuvMatrix::Bt709,
            };
            if index == 3 {
                frame.y_plane[24..].fill(255);
            }
            frame
        }

        // ceil(60/4)=15, encoded to the observed 16-row codec boundary.
        // The first three native bands stay 16 rows; only the last contributes
        // the remaining 12 visible rows.
        let mut compositor = SliceCompositor::new((2, 60));
        for index in 0..AVC_VIDEO_SLICE_COUNT {
            compositor
                .push(index, 1, Some(padded_slice(index)))
                .expect("native padded slice");
        }
        let frame = compositor
            .finish_frame()
            .expect("validated native padding")
            .expect("composite");
        assert_eq!((frame.width, frame.height), (2, 60));
        assert_eq!(
            frame
                .updates
                .iter()
                .map(|update| (update.y_origin, update.y_rows))
                .collect::<Vec<_>>(),
            vec![(0, 16), (16, 16), (32, 16), (48, 12)]
        );
        assert_eq!(frame.updates[3].uv_rows, 6);
    }

    #[test]
    fn rejects_a_crop_that_does_not_match_native_codec_alignment() {
        let mut compositor = SliceCompositor::new((2, 60));
        for index in 0..AVC_VIDEO_SLICE_COUNT {
            let frame = DecodedSlice {
                width: 2,
                height: 14,
                y_plane: vec![index as u8; 28],
                uv_plane: vec![128; 14],
                range: YuvRange::Video,
                matrix: YuvMatrix::Bt709,
            };
            compositor
                .push(index, 1, Some(frame))
                .expect("malformed slice");
        }
        let error = compositor
            .finish_frame()
            .expect_err("arbitrary undersized slices must fail");
        assert!(
            error.contains("expected_aligned_slice_height=16"),
            "{error}"
        );
    }
}
