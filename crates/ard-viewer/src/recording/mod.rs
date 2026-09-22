//! Session recording: writes the frames the viewer actually presents to an MP4
//! file.
//!
//! # What is recorded
//!
//! Recording is attached to the presentation boundary, not to the network or to
//! a decoder. [`crate::session_renderer`] copies the texture it is about to draw
//! — the MVS tile result, the RGB framebuffer, or the decoded AVC planes — into
//! a staging buffer on the GPU, and the frame that reaches the file is therefore
//! the same frame, at the same resolution, that was on screen. Re-decoding a
//! second time to record would be cheaper to write and would be wrong: the GPU
//! MVS path never produces CPU pixels at all, and any second decode can differ
//! from what the user saw.
//!
//! Each frame is stamped with the instant it was presented, so the recording is
//! variable-rate and playback holds every frame for exactly as long as the
//! viewer did. A static desktop is one long sample, not a repeated identical
//! frame; a slow window never shifts the frames that follow it.
//!
//! # Threading
//!
//! The render thread only encodes a texture-to-buffer copy, which costs no
//! readback stall. The staging buffer cannot be mapped in that same draw —
//! `wgpu` rejects mapping a buffer that a command buffer still being encoded
//! writes to, and iced submits that command buffer only after the draw returns —
//! so the frame is staged and mapped at the start of the next draw, when the
//! submission it belongs to has reached the queue.
//!
//! A dedicated recording thread then encodes each mapped frame with the platform
//! hardware encoder and appends the access unit to the MP4 file. It also polls
//! the device, because that poll is what delivers a mapped buffer. Staging
//! buffers are pooled and recycled by that thread, so a slow encoder drops frames
//! (and reports the count) instead of blocking the session, and stopping the
//! take waits for the staged frames before the file is finalized.
//!
//! On unsupported platforms the recorder reports a configuration error rather
//! than silently writing nothing.

#[allow(unsafe_code)]
mod encoder;
mod muxer;
/// Annex-B conversion is only needed by the Media Foundation encoder, but the
/// conversions are unit-tested wherever the crate is built.
#[cfg(any(target_os = "windows", test))]
mod nal;
mod tap;

pub(crate) use tap::CapturedFrame;
pub use tap::{FrameLayout, PresentationSource, RecordingTap};

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ard_rs::TakeOrigin;

use crate::i18n::Language;

pub(crate) use encoder::{EncodedSample, EncoderSettings, H264Encoder};
pub(crate) use muxer::{Mp4Muxer, VideoTrackFormat};

/// Timescale of the recorded video track: 90 kHz, the MP4 video standard.
///
/// It represents 24/25/30/50/60 fps exactly, so a recording does not drift away
/// from the wall clock the way a millisecond timescale would.
pub(crate) const TIMESCALE: u32 = 90_000;

/// Hint handed to the encoder's rate control.
pub(crate) const FRAME_RATE_HINT: u32 = 30;
/// Memory budget for the readback staging pool. A capture waits one draw to be
/// mapped and then one device poll to reach the encoder, so the pool has to hold
/// two or three frames; the budget keeps that bounded on a 4K display instead of
/// scaling the allocation with the frame size.
const STAGING_MEMORY_BUDGET: u64 = 192 * 1024 * 1024;
/// Smallest and largest number of staging buffers.
const MIN_STAGING_BUFFERS: usize = 3;
const MAX_STAGING_BUFFERS: usize = 6;
/// How often the recording thread drains finished access units and polls the
/// device while no new frame arrives. The poll is what delivers a mapped
/// staging buffer, so a short interval keeps the pool from filling up.
const SAMPLE_DRAIN_INTERVAL: Duration = Duration::from_millis(5);
/// How long stopping a take waits for the encoder flush and the movie index
/// before detaching the recording thread. A wedged encoder must not hang the UI.
const FINALIZE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a stopped take waits for the renderer to hand over the frames it had
/// already staged before the recording thread finalizes on its own.
const STOP_GRACE: Duration = Duration::from_secs(1);

/// Convert a timeline delta into track timescale units.
///
/// A sample may never have zero length: the container stores durations, so a
/// zero would stall playback on that frame and hide every later one.
pub(crate) fn timeline_units(delta: Duration) -> u32 {
    let units = delta
        .as_nanos()
        .saturating_mul(u128::from(TIMESCALE))
        .saturating_add(500_000)
        / 1_000_000_000;
    u32::try_from(units).unwrap_or(u32::MAX).max(1)
}

/// Convert an instant on the take timeline into track timescale units.
pub(crate) fn timeline_position(position: Duration) -> u64 {
    let units = position
        .as_nanos()
        .saturating_mul(u128::from(TIMESCALE))
        .saturating_add(500_000)
        / 1_000_000_000;
    u64::try_from(units).unwrap_or(u64::MAX)
}

/// Whether this build can record at all.
pub const fn available() -> bool {
    encoder::supported()
}

/// Encoder effort, expressed as the bits spent per pixel of presented content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingQuality {
    High,
    Balanced,
    Compact,
}

impl RecordingQuality {
    pub const ALL: [Self; 3] = [Self::High, Self::Balanced, Self::Compact];

    pub fn label(self, language: Language) -> &'static str {
        language.tr(match self {
            Self::High => "高画质",
            Self::Balanced => "标准",
            Self::Compact => "小体积",
        })
    }

    pub fn from_cache(value: &str) -> Self {
        match value {
            "high" => Self::High,
            "compact" => Self::Compact,
            _ => Self::Balanced,
        }
    }

    pub fn to_cache(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Balanced => "balanced",
            Self::Compact => "compact",
        }
    }

    fn bits_per_pixel(self) -> f64 {
        match self {
            Self::High => 0.20,
            Self::Balanced => 0.12,
            Self::Compact => 0.07,
        }
    }

    /// Average bitrate for a frame size.
    ///
    /// Screen content is mostly static, so the encoder spends far less than this
    /// on average; the target exists to bound how much detail a busy frame may
    /// keep. It is clamped so a tiny window still gets a usable stream and a
    /// huge one cannot ask for a bitrate no encoder accepts.
    pub(crate) fn bitrate(self, width: u32, height: u32) -> u32 {
        let pixels = f64::from(width) * f64::from(height) * f64::from(FRAME_RATE_HINT);
        let bitrate = pixels * self.bits_per_pixel();
        bitrate.clamp(1_500_000.0, 120_000_000.0) as u32
    }
}

/// Everything a take needs in order to write its first segment.
#[derive(Debug, Clone)]
pub struct RecordingConfig {
    pub directory: PathBuf,
    /// Human-readable prefix for the file name, usually the remote host.
    pub label: String,
    pub quality: RecordingQuality,
}

/// Default output directory: the user's movies folder, falling back to home.
pub fn default_directory() -> PathBuf {
    let Some(directories) = directories::UserDirs::new() else {
        return PathBuf::from("ARD Viewer");
    };
    let base = directories
        .video_dir()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| directories.home_dir().to_path_buf());
    base.join("ARD Viewer")
}

/// Reduce an endpoint to something safe to use as a file name.
pub fn sanitize_label(endpoint: &str) -> String {
    let mut label: String = endpoint
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || matches!(character, '.' | '-' | '_' | ' ') {
                character
            } else {
                '-'
            }
        })
        .collect();
    label = label
        .trim_matches(|character| matches!(character, '-' | ' '))
        .to_owned();
    // Ports are noise in a file name and every take records the same service.
    if let Some((host, port)) = label.rsplit_once('-')
        && !host.is_empty()
        && port.chars().all(|character| character.is_ascii_digit())
    {
        label = host.to_owned();
    }
    label.truncate(48);
    if label.is_empty() {
        "ard".to_owned()
    } else {
        label
    }
}

/// Path of one recording segment.
pub fn output_path(directory: &Path, label: &str, index: usize, segment: usize) -> PathBuf {
    let name = if segment <= 1 {
        format!("{label} {index}.mp4")
    } else {
        format!("{label} {index}-{segment}.mp4")
    };
    directory.join(name)
}

/// First unused recording index in `directory`.
pub fn next_index(directory: &Path, label: &str) -> usize {
    (1..=100_000)
        .find(|index| !output_path(directory, label, *index, 1).exists())
        .unwrap_or(1)
}

/// Shared counters a running take publishes to the UI.
#[derive(Debug)]
pub struct RecordingStats {
    captured: AtomicU64,
    dropped: AtomicU64,
    encoded: AtomicU64,
    written_bytes: AtomicU64,
    recorded_units: AtomicU64,
    /// 0 unknown, 1 hardware, 2 software.
    encoder_kind: AtomicU64,
    /// Instant at which the take's first frame was presented, which is also the
    /// zero of the video's timeline: anything that has to line up with the
    /// recording measures from here. Shared so the dump can read it from another
    /// thread without waiting for the take to end.
    origin: TakeOrigin,
    finished: AtomicBool,
    outcome: Mutex<RecordingOutcome>,
}

impl Default for RecordingStats {
    fn default() -> Self {
        Self {
            captured: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            encoded: AtomicU64::new(0),
            written_bytes: AtomicU64::new(0),
            recorded_units: AtomicU64::new(0),
            encoder_kind: AtomicU64::new(0),
            origin: TakeOrigin::new(),
            finished: AtomicBool::new(false),
            outcome: Mutex::new(RecordingOutcome::default()),
        }
    }
}

#[derive(Debug, Default, Clone)]
struct RecordingOutcome {
    segments: Vec<PathBuf>,
    error: Option<String>,
}

/// Snapshot of a take, for the UI and for a clean shutdown message.
#[derive(Debug, Clone, Default)]
pub struct RecordingProgress {
    pub captured_frames: u64,
    pub dropped_frames: u64,
    pub encoded_frames: u64,
    pub written_bytes: u64,
    /// Length of the recorded timeline.
    pub recorded: Duration,
    /// Whether the platform encoder reported hardware acceleration, once known.
    pub hardware_encoder: Option<bool>,
    pub finished: bool,
    pub segments: Vec<PathBuf>,
    pub error: Option<String>,
}

impl RecordingProgress {
    /// Wall-clock length implied by the encoded samples.
    pub fn recorded_seconds(&self) -> f64 {
        self.recorded.as_secs_f64()
    }
}

impl RecordingStats {
    fn snapshot(&self) -> RecordingProgress {
        let outcome = self
            .outcome
            .lock()
            .map(|outcome| outcome.clone())
            .unwrap_or_default();
        RecordingProgress {
            captured_frames: self.captured.load(Ordering::Relaxed),
            dropped_frames: self.dropped.load(Ordering::Relaxed),
            encoded_frames: self.encoded.load(Ordering::Relaxed),
            written_bytes: self.written_bytes.load(Ordering::Relaxed),
            recorded: Duration::from_nanos(
                self.recorded_units
                    .load(Ordering::Relaxed)
                    .saturating_mul(1_000_000_000)
                    / u64::from(TIMESCALE),
            ),
            hardware_encoder: match self.encoder_kind.load(Ordering::Relaxed) {
                1 => Some(true),
                2 => Some(false),
                _ => None,
            },
            finished: self.finished.load(Ordering::Acquire),
            segments: outcome.segments,
            error: outcome.error,
        }
    }

    pub(crate) fn record_captured_frame(&self) {
        self.captured.fetch_add(1, Ordering::Relaxed);
    }

    /// The take's origin, shared with the development dump so both measure from
    /// the video's own zero.
    pub fn take_origin(&self) -> TakeOrigin {
        self.origin.clone()
    }

    pub(crate) fn record_dropped_frame(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }

    fn record_encoder_kind(&self, hardware: Option<bool>) {
        let value = match hardware {
            Some(true) => 1,
            Some(false) => 2,
            None => 0,
        };
        self.encoder_kind.store(value, Ordering::Relaxed);
    }

    fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    fn mark_finished(&self, outcome: RecordingOutcome) {
        if let Ok(mut stored) = self.outcome.lock() {
            *stored = outcome;
        }
        self.finished.store(true, Ordering::Release);
    }
}

/// One recording take: the frames the renderer should hand over, and where they
/// are going.
#[derive(Debug)]
pub struct Take {
    id: u64,
    started: Instant,
    stats: Arc<RecordingStats>,
    frames: Mutex<Option<Sender<CapturedFrame>>>,
    device: Mutex<Option<wgpu::Device>>,
    /// Set when the take is asked to stop. The renderer keeps capturing nothing
    /// further and hands over the frames it has already staged; the recording
    /// thread finalizes once that has happened (or once the grace period passes,
    /// so a window that never redraws cannot hold the file open).
    stopping: AtomicBool,
}

impl Take {
    fn new(id: u64, stats: Arc<RecordingStats>, sender: Sender<CapturedFrame>) -> Self {
        Self {
            id,
            started: Instant::now(),
            stats,
            frames: Mutex::new(Some(sender)),
            device: Mutex::new(None),
            stopping: AtomicBool::new(false),
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    /// Timeline position of `now`, measured from the first captured frame.
    pub fn position(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.started)
    }

    /// A sender for one captured frame, or `None` once the take is closing.
    pub(crate) fn sender(&self) -> Option<Sender<CapturedFrame>> {
        self.frames.lock().ok().and_then(|frames| frames.clone())
    }

    pub(crate) fn stats(&self) -> Arc<RecordingStats> {
        Arc::clone(&self.stats)
    }

    /// Publish the render device so the recording thread can drive its own
    /// device polls: a mapped staging buffer is only delivered once someone
    /// polls the device, and waiting for a redraw would stall the file.
    pub(crate) fn publish_device(&self, device: &wgpu::Device) {
        if let Ok(mut stored) = self.device.lock()
            && stored.is_none()
        {
            *stored = Some(device.clone());
        }
    }

    fn device(&self) -> Option<wgpu::Device> {
        self.device.lock().ok().and_then(|device| device.clone())
    }

    pub(crate) fn record_captured_frame(&self) {
        self.stats.record_captured_frame();
    }

    pub(crate) fn record_dropped_frame(&self) {
        self.stats.record_dropped_frame();
    }

    pub(crate) fn request_stop(&self) {
        self.stopping.store(true, Ordering::Release);
    }

    pub(crate) fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }

    fn close(&self) {
        if let Ok(mut frames) = self.frames.lock() {
            *frames = None;
        }
    }
}

/// Shared switch between the UI, the renderer and the recording thread.
///
/// The renderer keeps one of these for the lifetime of a session window and asks
/// it whether a take is running, so starting a recording never has to rebuild
/// the GPU pipeline.
#[derive(Debug, Default)]
pub struct RecordingControl {
    take: Mutex<Option<Arc<Take>>>,
    next_id: AtomicU64,
}

impl RecordingControl {
    pub fn new() -> Self {
        Self {
            take: Mutex::new(None),
            next_id: AtomicU64::new(1),
        }
    }

    /// The take the renderer should capture into, if any.
    pub fn active(&self) -> Option<Arc<Take>> {
        self.take.lock().ok().and_then(|take| take.clone())
    }

    fn publish(&self, take: Arc<Take>) {
        if let Ok(mut current) = self.take.lock() {
            *current = Some(take);
        }
    }

    /// Ask the active take to stop capturing.
    ///
    /// The take stays published so the renderer can still hand over the frames
    /// it staged for the presentation that is already in flight; it is dropped
    /// by [`RecordingControl::clear`] once the file is finalized.
    fn request_stop(&self) {
        if let Some(take) = self.active() {
            take.request_stop();
        }
    }

    /// Drop the active take, which closes the frame channel.
    fn clear(&self) {
        let take = self.take.lock().ok().and_then(|mut take| take.take());
        if let Some(take) = take {
            take.close();
        }
    }

    fn allocate_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }
}

/// A recording owned by the application.
///
/// Dropping it stops the take, flushes the encoder and writes the movie index,
/// so quitting the viewer never leaves a half-written file behind.
#[derive(Debug)]
pub struct Recorder {
    control: Arc<RecordingControl>,
    stats: Arc<RecordingStats>,
    started: Instant,
    thread: Option<JoinHandle<()>>,
}

impl Recorder {
    pub fn start(control: Arc<RecordingControl>, config: RecordingConfig) -> Result<Self, String> {
        if !available() {
            return Err("当前平台没有系统 H.264 编码器，无法录制会话".into());
        }
        fs::create_dir_all(&config.directory)
            .map_err(|error| format!("无法创建录制目录 {}：{error}", config.directory.display()))?;
        let index = next_index(&config.directory, &config.label);
        let stats = Arc::new(RecordingStats::default());
        let (sender, receiver) = std::sync::mpsc::channel();
        let take = Arc::new(Take::new(control.allocate_id(), Arc::clone(&stats), sender));
        // The renderer must be able to see the take before the thread starts,
        // but the thread must not start before the take is published either.
        control.publish(Arc::clone(&take));
        let thread_config = config.clone();
        let thread_stats = Arc::clone(&stats);
        let thread_take = Arc::clone(&take);
        let thread = thread::Builder::new()
            .name("ard-recording".into())
            .spawn(move || run_recording(thread_take, thread_config, index, thread_stats, receiver))
            .map_err(|error| {
                control.clear();
                format!("无法启动录制线程：{error}")
            })?;
        Ok(Self {
            control,
            stats,
            started: Instant::now(),
            thread: Some(thread),
        })
    }

    /// Ask the take to stop without waiting.
    ///
    /// Stopping is deliberately asynchronous: the frames already staged for the
    /// presentation in flight are only handed over on the next redraw, which
    /// cannot happen while the UI thread is blocked. [`Recorder::progress`]
    /// reports `finished` once the file is complete.
    pub fn request_stop(&self) {
        self.control.request_stop();
    }

    /// Stop the take and wait for the file to be finalized.
    ///
    /// The wait is bounded: a take that cannot flush (a wedged encoder, a GPU
    /// device that stopped making progress) must not freeze the window, so the
    /// thread is detached once the deadline passes. Callers that can keep
    /// rendering should prefer [`Recorder::request_stop`] so the last staged
    /// frame still reaches the file.
    pub fn stop(&mut self) {
        self.request_stop();
        self.wait_for_finish(FINALIZE_TIMEOUT);
    }

    fn wait_for_finish(&mut self, timeout: Duration) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        let deadline = Instant::now() + timeout;
        while !self.stats.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if self.stats.is_finished() {
            let _ = thread.join();
        } else {
            // Detached: the thread finalizes on its own grace period, and the
            // file is completed by the process exit rather than by this wait.
            self.control.clear();
            drop(thread);
        }
    }

    /// The instant the take's first frame was presented, shared with whoever has
    /// to measure from the same zero as the recorded video.
    ///
    /// It stays empty until the first frame arrives, so a reader has to tolerate
    /// `None` at the very start of a take.
    pub fn take_origin(&self) -> TakeOrigin {
        self.stats.take_origin()
    }

    pub fn progress(&self) -> RecordingProgress {
        let mut progress = self.stats.snapshot();
        if !progress.finished {
            // While the take runs, the badge shows wall-clock time, which is
            // what the person who pressed record is watching.
            progress.recorded = self.started.elapsed();
        }
        progress
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop();
    }
}

/// One encoded segment: a size or pixel-format change starts a new file rather
/// than scaling, cropping or re-encoding frames that no longer match.
struct Segment {
    width: u32,
    height: u32,
    nv12: bool,
    encoder: H264Encoder,
    muxer: Mp4Muxer,
}

impl Segment {
    fn start(
        config: &RecordingConfig,
        index: usize,
        segment: usize,
        layout: &FrameLayout,
    ) -> Result<Self, String> {
        let (width, height) = (layout.width(), layout.height());
        let path = output_path(&config.directory, &config.label, index, segment);
        let encoder = H264Encoder::new(EncoderSettings {
            width,
            height,
            bitrate: config.quality.bitrate(width, height),
            frame_rate_hint: FRAME_RATE_HINT,
            // The recorder hands the decoded planes through unchanged, so the
            // recording has to carry the same colour description they are
            // encoded in rather than a fixed BT.709 guess.
            primaries: layout.primaries().unwrap_or_default(),
        })?;
        let muxer = Mp4Muxer::create(&path)?;
        Ok(Self {
            width,
            height,
            nv12: layout.is_nv12(),
            encoder,
            muxer,
        })
    }

    fn matches(&self, layout: &FrameLayout) -> bool {
        layout.width() == self.width
            && layout.height() == self.height
            && layout.is_nv12() == self.nv12
            && layout.primaries().unwrap_or_default() == self.encoder.settings().primaries
    }
}

/// Publish the timeline and file size of every segment written so far.
fn publish_totals(
    stats: &RecordingStats,
    completed_units: u64,
    completed_bytes: u64,
    muxer: &Mp4Muxer,
) {
    stats
        .written_bytes
        .store(completed_bytes + muxer.file_bytes(), Ordering::Relaxed);
    stats
        .recorded_units
        .store(completed_units + muxer.duration_units(), Ordering::Relaxed);
}

fn run_recording(
    take: Arc<Take>,
    config: RecordingConfig,
    index: usize,
    stats: Arc<RecordingStats>,
    receiver: Receiver<CapturedFrame>,
) {
    let mut state = RecordingState::new(config, index, Arc::clone(&stats));
    let mut idle_since = Instant::now();
    loop {
        // A mapped staging buffer is only delivered once the device is polled,
        // and the renderer may be idle with a static remote desktop, so the
        // recording thread drives the poll itself.
        if let Some(device) = take.device() {
            let _ = device.poll(wgpu::PollType::Poll);
        }
        match receiver.recv_timeout(SAMPLE_DRAIN_INTERVAL) {
            Ok(frame) => {
                idle_since = Instant::now();
                if let Err(error) = state.push(frame) {
                    stats.mark_finished(RecordingOutcome {
                        segments: state.segments(),
                        error: Some(error),
                    });
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                // A stopped take whose renderer never redraws again must still
                // produce a finished file instead of holding it open.
                if take.is_stopping() && idle_since.elapsed() >= STOP_GRACE {
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
        // Access units complete asynchronously, so drain on every wake-up as
        // well as after every frame.
        if let Err(error) = state.drain() {
            stats.mark_finished(RecordingOutcome {
                segments: state.segments(),
                error: Some(error),
            });
            return;
        }
    }
    let outcome = state.finish();
    stats.mark_finished(outcome);
}

struct RecordingState {
    config: RecordingConfig,
    index: usize,
    stats: Arc<RecordingStats>,
    segment: Option<Segment>,
    segment_number: usize,
    segments: Vec<PathBuf>,
    samples: Vec<EncodedSample>,
    /// Position and wall-clock instant of the most recently captured frame, so
    /// the last frame can be closed with the time it actually stayed on screen.
    last_frame: Option<(Duration, Instant)>,
    /// Totals of every closed segment, so the published progress covers all of
    /// them rather than only the segment being written.
    completed_units: u64,
    completed_bytes: u64,
}

impl RecordingState {
    fn new(config: RecordingConfig, index: usize, stats: Arc<RecordingStats>) -> Self {
        Self {
            config,
            index,
            stats,
            segment: None,
            segment_number: 1,
            segments: Vec::new(),
            samples: Vec::new(),
            last_frame: None,
            completed_units: 0,
            completed_bytes: 0,
        }
    }

    fn segments(&self) -> Vec<PathBuf> {
        self.segments.clone()
    }

    fn push(&mut self, frame: CapturedFrame) -> Result<(), String> {
        // The first frame of the take defines the take's origin, which is also
        // the zero of the recorded video's timeline. The development dump reads
        // this same instant, so a dump entry and a video frame share one clock.
        self.stats.origin.set(frame.presented);
        let layout = frame.layout;
        let needs_segment = match self.segment.as_ref() {
            Some(segment) => !segment.matches(&layout),
            None => true,
        };
        if needs_segment {
            self.close_segment()?;
            let segment = Segment::start(&self.config, self.index, self.segment_number, &layout)?;
            self.stats
                .record_encoder_kind(segment.encoder.hardware_accelerated());
            self.segment = Some(segment);
        }
        let segment = self.segment.as_mut().expect("segment was just created");
        let pts = frame.pts;
        let pool = Arc::clone(&frame.pool);
        let result = {
            // Keep the mapped view alive only for the copy into the encoder's
            // pixel buffer; the staging buffer goes straight back to the pool.
            let view = frame.buffer.slice(..).get_mapped_range();
            let source = layout.source_frame(&view);
            match source {
                Some(source) => segment.encoder.push(source, pts),
                None => Err("录制帧的缓冲区布局无效".into()),
            }
        };
        frame.buffer.unmap();
        pool.release(frame.buffer);
        result?;
        // `captured` is counted by the tap, which sees frames the encoder may
        // still drop; counting here as well would double every frame.
        self.last_frame = Some((pts, Instant::now()));
        self.drain()
    }

    fn drain(&mut self) -> Result<(), String> {
        let Some(segment) = self.segment.as_mut() else {
            return Ok(());
        };
        segment.encoder.take_samples(&mut self.samples);
        if self.samples.is_empty() {
            return Ok(());
        }
        let Some(sets) = segment.encoder.parameter_sets() else {
            // The parameter sets arrive with the first sample; anything before
            // that cannot be described by an avcC configuration box.
            self.samples.clear();
            return Ok(());
        };
        let format = VideoTrackFormat {
            width: segment.width,
            height: segment.height,
            sps: sets.sps,
            pps: sets.pps,
        };
        let mut written = 0_u64;
        for sample in self.samples.drain(..) {
            segment.muxer.write_sample(&format, sample)?;
            written += 1;
        }
        self.stats.encoded.fetch_add(written, Ordering::Relaxed);
        publish_totals(
            &self.stats,
            self.completed_units,
            self.completed_bytes,
            &segment.muxer,
        );
        Ok(())
    }

    /// Flush the encoder and close the current segment.
    fn close_segment(&mut self) -> Result<(), String> {
        let Some(mut segment) = self.segment.take() else {
            return Ok(());
        };
        let now = self.take_end_position();
        segment.encoder.finish(now)?;
        // The segment must be back in place for the drain to see it.
        self.segment = Some(segment);
        self.drain()?;
        let mut segment = self.segment.take().expect("segment was restored");
        let path = segment.muxer.path().to_path_buf();
        if segment.muxer.samples() == 0 {
            // Nothing was captured for this size: do not leave an empty file.
            drop(segment.muxer);
            fs::remove_file(&path).ok();
            return Ok(());
        }
        let summary = segment.muxer.finalize()?;
        self.completed_units = self.completed_units.saturating_add(summary.duration_units);
        self.completed_bytes = self.completed_bytes.saturating_add(summary.bytes);
        self.stats
            .recorded_units
            .store(self.completed_units, Ordering::Relaxed);
        self.stats
            .written_bytes
            .store(self.completed_bytes, Ordering::Relaxed);
        self.segments.push(summary.path);
        self.segment_number += 1;
        Ok(())
    }

    /// Timeline position at which the frame still held by the encoder stopped
    /// being on screen.
    ///
    /// The held frame was presented at the last captured frame's position and
    /// stayed there until the take ended, so the encoded timeline ends where the
    /// recording actually ended rather than at a nominal frame duration.
    fn take_end_position(&self) -> Duration {
        match self.last_frame {
            Some((pts, captured_at)) => pts.saturating_add(captured_at.elapsed()),
            None => Duration::ZERO,
        }
    }

    fn finish(&mut self) -> RecordingOutcome {
        let mut error = None;
        if let Err(failure) = self.close_segment()
            && error.is_none()
        {
            error = Some(failure);
        }
        if error.is_none() && self.segments.is_empty() {
            error = Some("录制没有捕获到任何画面".into());
        }
        RecordingOutcome {
            segments: std::mem::take(&mut self.segments),
            error,
        }
    }
}

/// A pool of mapped-for-readback staging buffers shared with the recording
/// thread, which returns each buffer once its frame is encoded.
#[derive(Debug)]
pub(crate) struct StagingPool {
    size: u64,
    free: Mutex<Vec<Arc<wgpu::Buffer>>>,
    created: AtomicU64,
}

impl StagingPool {
    pub(crate) fn new(size: u64) -> Arc<Self> {
        Arc::new(Self {
            size,
            free: Mutex::new(Vec::new()),
            created: AtomicU64::new(0),
        })
    }

    /// A buffer ready to receive a texture copy, or `None` when the encoder is
    /// behind and every buffer is still in flight.
    pub(crate) fn acquire(&self, device: &wgpu::Device) -> Option<Arc<wgpu::Buffer>> {
        let mut free = self.free.lock().ok()?;
        if let Some(buffer) = free.pop() {
            return Some(buffer);
        }
        let limit = (STAGING_MEMORY_BUDGET / self.size.max(1))
            .clamp(MIN_STAGING_BUFFERS as u64, MAX_STAGING_BUFFERS as u64);
        if self.created.load(Ordering::Relaxed) >= limit {
            return None;
        }
        self.created.fetch_add(1, Ordering::Relaxed);
        drop(free);
        Some(Arc::new(device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ARD recording readback"),
            size: self.size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        })))
    }

    pub(crate) fn release(&self, buffer: Arc<wgpu::Buffer>) {
        if let Ok(mut free) = self.free.lock() {
            free.push(buffer);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use super::{
        FRAME_RATE_HINT, FrameLayout, RecordingQuality, TIMESCALE, encoder, muxer, next_index,
        output_path, sanitize_label, timeline_position, timeline_units,
    };
    use crate::media::{YuvMatrix, YuvPrimaries, YuvRange};

    #[test]
    fn timeline_units_represent_common_frame_rates_exactly() {
        assert_eq!(timeline_units(Duration::from_nanos(16_666_667)), 1500);
        assert_eq!(timeline_units(Duration::from_nanos(33_333_333)), 3000);
        assert_eq!(timeline_units(Duration::from_millis(20)), 1800);
        assert_eq!(timeline_units(Duration::from_secs(1)), TIMESCALE);
        assert_eq!(
            timeline_position(Duration::from_secs(1)),
            u64::from(TIMESCALE)
        );
    }

    #[test]
    fn sub_tick_deltas_still_produce_a_playable_sample() {
        assert_eq!(timeline_units(Duration::ZERO), 1);
        assert_eq!(timeline_units(Duration::from_nanos(1)), 1);
        assert_eq!(timeline_position(Duration::ZERO), 0);
    }

    #[test]
    fn labels_become_safe_file_names() {
        assert_eq!(sanitize_label("192.168.0.24:5900"), "192.168.0.24");
        assert_eq!(sanitize_label("mac-mini.local:5900"), "mac-mini.local");
        assert_eq!(sanitize_label("wei@mac/studio"), "wei-mac-studio");
        assert_eq!(sanitize_label("  "), "ard");
        assert_eq!(sanitize_label(&"h".repeat(200)).len(), 48);
    }

    #[test]
    fn recording_files_are_numbered_without_overwriting() {
        let directory = std::env::temp_dir().join(format!(
            "ard-recording-names-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&directory).expect("temp directory");
        assert_eq!(next_index(&directory, "host"), 1);
        std::fs::write(output_path(&directory, "host", 1, 1), b"first").expect("write");
        assert_eq!(next_index(&directory, "host"), 2);
        // A continuation segment of take 1 does not shift the next take.
        std::fs::write(output_path(&directory, "host", 1, 2), b"second").expect("write");
        assert_eq!(next_index(&directory, "host"), 2);
        assert_eq!(
            output_path(&directory, "host", 2, 2)
                .file_name()
                .expect("file name")
                .to_string_lossy(),
            "host 2-2.mp4"
        );
        std::fs::remove_dir_all(&directory).ok();
    }

    /// Packed RGBA, the format the viewer's presentation texture holds.
    fn test_frame(width: u32, height: u32, index: u32) -> Vec<u8> {
        let mut frame = vec![0_u8; (width * height * 4) as usize];
        for y in 0..height {
            for x in 0..width {
                let offset = ((y * width + x) * 4) as usize;
                frame[offset] = (x * 255 / width.max(1)) as u8;
                frame[offset + 1] = (y * 255 / height.max(1)) as u8;
                frame[offset + 2] = 96;
                frame[offset + 3] = 255;
            }
        }
        let patch = 48_u32;
        let left = (index * 29) % width.saturating_sub(patch).max(1);
        let top = (index * 17) % height.saturating_sub(patch).max(1);
        for y in top..(top + patch).min(height) {
            for x in left..(left + patch).min(width) {
                let offset = ((y * width + x) * 4) as usize;
                frame[offset..offset + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
        frame
    }

    /// The staging layout the capture tap hands the encoder: the presentation
    /// texture swizzled to BGRA, which is what the hardware encoders take.
    fn to_bgra(rgba: &[u8]) -> Vec<u8> {
        let mut bgra = rgba.to_vec();
        for pixel in bgra.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        bgra
    }

    fn run_tool(program: &str, arguments: &[&str]) -> Option<Vec<u8>> {
        let output = std::process::Command::new(program)
            .args(arguments)
            .output()
            .ok()?;
        output
            .status
            .success()
            .then_some(output.stdout)
            .or_else(|| {
                eprintln!(
                    "{program} failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                None
            })
    }

    /// The recorder's whole promise is that the file holds the frames that were
    /// presented. This test writes real frames through the real platform
    /// encoder and container, then decodes the result with an independent
    /// decoder (ffmpeg) and compares every frame against its source.
    #[test]
    fn recorded_file_holds_the_presented_frames() {
        if !encoder::supported() {
            eprintln!("skipping: this platform has no system H.264 encoder");
            return;
        }
        if run_tool("ffmpeg", &["-version"]).is_none() {
            eprintln!("skipping: ffmpeg is not installed");
            return;
        }
        let width = 320_u32;
        let height = 240_u32;
        // A 25 fps start followed by a two-second static period: the timeline is
        // variable-rate exactly like a real session.
        let timeline_ms = [0_u64, 40, 80, 2_000, 2_040];
        let frames: Vec<Vec<u8>> = (0..timeline_ms.len() as u32)
            .map(|index| test_frame(width, height, index))
            .collect();

        let directory = std::env::temp_dir().join(format!(
            "ard-recording-encode-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&directory).expect("temp directory");
        let path = directory.join("recording.mp4");

        let mut muxer = muxer::Mp4Muxer::create(&path).expect("muxer starts");
        let mut encoder = encoder::H264Encoder::new(encoder::EncoderSettings {
            width,
            height,
            // A high target keeps the comparison about structure, not rate control.
            bitrate: 40_000_000,
            frame_rate_hint: 25,
            primaries: crate::media::YuvPrimaries::Bt709,
        })
        .expect("encoder starts");
        let mut samples = Vec::new();
        let staged: Vec<Vec<u8>> = frames.iter().map(|frame| to_bgra(frame)).collect();
        for (index, bytes) in staged.iter().enumerate() {
            encoder
                .push(
                    encoder::SourceFrame::Bgra {
                        stride: width as usize * 4,
                        bytes,
                    },
                    Duration::from_millis(timeline_ms[index]),
                )
                .expect("frame accepted");
            encoder.take_samples(&mut samples);
            let sets = encoder.parameter_sets();
            for sample in samples.drain(..) {
                let sets = sets
                    .clone()
                    .expect("parameter sets follow the first sample");
                muxer
                    .write_sample(
                        &muxer::VideoTrackFormat {
                            width,
                            height,
                            sps: sets.sps,
                            pps: sets.pps,
                        },
                        sample,
                    )
                    .expect("sample written");
            }
        }
        encoder
            .finish(Duration::from_millis(
                *timeline_ms.last().expect("timeline") + 40,
            ))
            .expect("encoder flushes");
        encoder.take_samples(&mut samples);
        let sets = encoder.parameter_sets().expect("parameter sets");
        for sample in samples.drain(..) {
            muxer
                .write_sample(
                    &muxer::VideoTrackFormat {
                        width,
                        height,
                        sps: sets.sps.clone(),
                        pps: sets.pps.clone(),
                    },
                    sample,
                )
                .expect("sample written");
        }
        let summary = muxer.finalize().expect("finalized");
        assert_eq!(summary.path, path);

        // 1. An independent demuxer must accept the container and its timing.
        let probe = run_tool(
            "ffprobe",
            &[
                "-v",
                "error",
                "-print_format",
                "json",
                "-show_streams",
                "-show_packets",
                path.to_str().expect("utf-8 path"),
            ],
        )
        .expect("ffprobe reads the recording");
        let probe: serde_json::Value = serde_json::from_slice(&probe).expect("ffprobe emits json");
        let stream = &probe["streams"][0];
        assert_eq!(stream["codec_name"], "h264");
        assert_eq!(stream["width"], width);
        assert_eq!(stream["height"], height);
        assert_eq!(stream["nb_read_packets"], timeline_ms.len().to_string());
        let packets = probe["packets"].as_array().expect("packet list");
        assert_eq!(packets.len(), timeline_ms.len());
        for (index, packet) in packets.iter().enumerate() {
            let pts = packet["pts_time"]
                .as_str()
                .expect("pts")
                .parse::<f64>()
                .expect("numeric pts");
            let expected = timeline_ms[index] as f64 / 1_000.0;
            assert!(
                (pts - expected).abs() < 0.002,
                "frame {index} is presented at {pts}s instead of {expected}s"
            );
        }

        // 2. Every decoded frame must still be the frame that was presented.
        // `-fps_mode passthrough` is essential: without it ffmpeg fills the
        // two-second static period with duplicated frames and the frame count
        // would say nothing about what was recorded.
        let decoded = run_tool(
            "ffmpeg",
            &[
                "-v",
                "error",
                "-i",
                path.to_str().expect("utf-8 path"),
                "-fps_mode",
                "passthrough",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgba",
                "-",
            ],
        )
        .expect("ffmpeg decodes the recording");
        let frame_bytes = (width * height * 4) as usize;
        assert_eq!(
            decoded.len(),
            frame_bytes * frames.len(),
            "the recording must hold exactly the presented frames"
        );
        for (index, source) in frames.iter().enumerate() {
            let decoded = &decoded[index * frame_bytes..(index + 1) * frame_bytes];
            let mut differences = Vec::with_capacity(source.len() / 4 * 3);
            for (expected, actual) in source.chunks_exact(4).zip(decoded.chunks_exact(4)) {
                // Alpha is not part of the video and is not compared.
                for channel in 0..3 {
                    differences.push(u32::from(expected[channel].abs_diff(actual[channel])));
                }
            }
            let total: u64 = differences.iter().map(|value| u64::from(*value)).sum();
            let mean = total as f64 / differences.len() as f64;
            differences.sort_unstable();
            let p99 = differences[differences.len() * 99 / 100];
            // Measured on this content: mean 0.66 and p99 of 2. The bounds are
            // still far tighter than any real defect (a colour swap, a half-pixel
            // shift, a scaled or filtered frame) would produce.
            assert!(
                mean < 1.5,
                "frame {index} differs from the presented frame by {mean:.2} on average"
            );
            assert!(
                p99 <= 8,
                "frame {index} has a 99th-percentile channel error of {p99}"
            );
        }

        std::fs::remove_dir_all(&directory).ok();
    }

    /// Frame size of one segment of a rebuild.
    #[derive(Default)]
    struct RebuildSegment {
        width: u32,
        height: u32,
    }

    /// Development tool: rebuild a video from a dump of a take, whichever stream
    /// the take carried.
    ///
    /// The dump holds decrypted server data, so replaying it through the same
    /// dispatcher, assembler and decoders the viewer used reproduces the frames
    /// the viewer presented, stamped with the instants the dump recorded. That
    /// makes the rebuilt video comparable, frame for frame, with the recording
    /// it came from.
    ///
    /// ```text
    /// ARD_RAW_STREAM_REBUILD="/path/host server stream.jsonl=/tmp/rebuilt.mp4" \
    /// ARD_RAW_STREAM_BITRATE=80000000 \
    ///   cargo test -p ard-viewer --bin ard-viewer rebuild_from_raw_stream \
    ///     -- --ignored --nocapture
    /// ```
    ///
    /// The same command rebuilds either stream: a record dump through the RFB
    /// decoder, a video dump through the RTP assembler and the media decoder.
    #[test]
    #[ignore = "development tool: set ARD_RAW_STREAM_REBUILD=<index.jsonl>=<out.mp4>"]
    fn rebuild_from_raw_stream() {
        let Ok(request) = std::env::var("ARD_RAW_STREAM_REBUILD") else {
            eprintln!("set ARD_RAW_STREAM_REBUILD=<index.jsonl>=<out.mp4>");
            return;
        };
        let (index, output) = request
            .split_once('=')
            .expect("ARD_RAW_STREAM_REBUILD=<index.jsonl>=<out.mp4>");
        let bitrate = std::env::var("ARD_RAW_STREAM_BITRATE")
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
            .unwrap_or_else(|| RecordingQuality::High.bitrate(1920, 1080));
        let summary =
            rebuild_dump(Path::new(index), Path::new(output), bitrate).expect("the dump rebuilds");
        eprintln!(
            "rebuilt {} frames from {index} into {output} ({} bytes)",
            summary.frames, summary.bytes
        );
    }

    /// Rebuild whichever stream a dump holds.
    fn rebuild_dump(
        index_path: &Path,
        output: &Path,
        bitrate: u32,
    ) -> Result<RebuildSummary, String> {
        let index = ard_rs::RawStreamIndex::read(index_path)?;
        let media = index
            .records
            .first()
            .is_some_and(|record| record.framing == ard_rs::RecordFraming::UdpPacket);
        if !media {
            return rebuild_record_stream(&index, index_path, output, bitrate);
        }
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            rebuild_media_stream_dump(&index, index_path, output, bitrate)
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let _ = (index_path, output, bitrate);
            Err("当前平台没有 AVC/HEVC 解码器，无法重建媒体流裸流".into())
        }
    }

    /// What one rebuild produced.
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
    struct RebuildSummary {
        frames: u64,
        bytes: u64,
    }

    /// Replay a record stream dump into a video.
    ///
    /// The records are decoded exactly as the live session decoded them — the
    /// same dispatcher, the same CPU decoder — and each frame is written with the
    /// arrival time the dump recorded for it, which is the recording's own clock.
    /// A size change starts a new segment, the way a recording does.
    fn rebuild_record_stream(
        index: &ard_rs::RawStreamIndex,
        index_path: &Path,
        output: &Path,
        bitrate: u32,
    ) -> Result<RebuildSummary, String> {
        use ard_rs::{Decoder, Framebuffer, FramebufferFormat, PixelFormat};

        if index.records.is_empty() {
            return Err("裸流没有任何记录".into());
        }
        if index
            .records
            .iter()
            .any(|record| record.framing != ard_rs::RecordFraming::TcpRecord)
        {
            return Err(format!(
                "{} 不是记录流裸流；RTP 裸流需要媒体管线重建",
                index_path.display()
            ));
        }
        let raw = std::fs::read(ard_rs::RawStreamIndex::raw_path(index_path))
            .map_err(|error| format!("无法读取裸流：{error}"))?;

        let mut decoder = Decoder::new(PixelFormat::XRGB8888).map_err(|error| error.to_string())?;
        let mut framebuffer =
            Framebuffer::new_with_format(1, 1, FramebufferFormat::Native(PixelFormat::XRGB8888))
                .map_err(|error| error.to_string())?;
        let mut dispatcher = ard_rs::ArdMessageDispatcher::new(64 * 1024 * 1024, 1024 * 1024)
            .map_err(|error| error.to_string())?;

        let mut segments = Vec::new();
        let mut current: Option<(RebuildSegment, encoder::H264Encoder, muxer::Mp4Muxer)> = None;
        let mut samples = Vec::new();
        let mut frames = 0_u64;

        for (number, record) in index.records.iter().enumerate() {
            let start = record.offset as usize;
            let end = start
                .checked_add(record.length as usize)
                .ok_or("记录超出裸流范围")?;
            let payload = raw
                .get(start..end)
                .ok_or_else(|| format!("记录 {number} 超出裸流范围"))?;
            let messages = dispatcher
                .push(payload, &mut decoder, &mut framebuffer)
                .map_err(|error| format!("记录 {number} 无法解码：{error}"))?;
            if !messages.iter().any(|message| {
                matches!(message, ard_rs::ArdServerMessage::FramebufferUpdate { .. })
            }) {
                continue;
            }
            let (width, height) = (
                u32::from(framebuffer.width()),
                u32::from(framebuffer.height()),
            );
            if width == 0 || height == 0 {
                continue;
            }
            if current
                .as_ref()
                .is_some_and(|(segment, _, _)| segment.width != width || segment.height != height)
                && let Some((segment, mut encoder, mut muxer)) = current.take()
            {
                encoder.finish(Duration::from_millis(record.t))?;
                encoder.take_samples(&mut samples);
                write_rebuild_samples(&mut muxer, &encoder, &mut samples, &segment)?;
                segments.push(muxer.finalize()?);
            }
            if current.is_none() {
                current = Some((
                    RebuildSegment { width, height },
                    encoder::H264Encoder::new(encoder::EncoderSettings {
                        width,
                        height,
                        bitrate,
                        frame_rate_hint: FRAME_RATE_HINT,
                        primaries: crate::media::YuvPrimaries::Bt709,
                    })?,
                    muxer::Mp4Muxer::create(output)?,
                ));
            }
            let mut rgba = Vec::new();
            if !crate::session_runtime::framebuffer_to_rgba(&framebuffer, &mut rgba) {
                return Err(format!("记录 {number} 解码出的 framebuffer 无法呈现"));
            }
            // The viewer presents RGBA; the encoder takes the same frame
            // swizzled to BGRA, exactly as the capture tap hands it over: rows
            // padded to the copy alignment, which is what the staging buffer
            // holds.
            let bgra = to_bgra(&rgba);
            let layout = FrameLayout::bgra(width, height);
            let stride = match layout {
                FrameLayout::Bgra { stride, .. } => stride as usize,
                FrameLayout::Nv12 { .. } => unreachable!("the rebuild stages BGRA frames"),
            };
            let row_bytes = width as usize * 4;
            let mut staging = vec![0_u8; layout.buffer_size() as usize];
            for (row, source_row) in bgra.chunks_exact(row_bytes).enumerate() {
                let start = row * stride;
                staging[start..start + row_bytes].copy_from_slice(source_row);
            }
            let source = layout
                .source_frame(&staging)
                .ok_or_else(|| format!("记录 {number} 的帧与布局不匹配"))?;
            let (segment, encoder, muxer) = current.as_mut().expect("a segment was just started");
            encoder.push(source, Duration::from_millis(record.t))?;
            encoder.take_samples(&mut samples);
            write_rebuild_samples(muxer, encoder, &mut samples, segment)?;
            frames += 1;
        }

        if let Some((segment, mut encoder, mut muxer)) = current.take() {
            let end = index
                .records
                .last()
                .map(|record| record.t)
                .unwrap_or_default();
            encoder.finish(Duration::from_millis(end))?;
            encoder.take_samples(&mut samples);
            write_rebuild_samples(&mut muxer, &encoder, &mut samples, &segment)?;
            segments.push(muxer.finalize()?);
        }
        if frames == 0 {
            return Err("裸流没有解码出任何画面".into());
        }

        Ok(RebuildSummary {
            frames,
            bytes: segments.iter().map(|summary| summary.bytes).sum(),
        })
    }

    /// Rebuild a video from a dump of the take's UDP media stream.
    ///
    /// Only the platforms with a media decoder can do this: the rebuild is the
    /// live decode path, fed from a file instead of a socket.
    ///
    /// The dumped packets go through the same RTP assembler the live receiver
    /// uses, so the bands of each desktop frame come out in the order the
    /// receiver released them, and then through the same platform decoder and
    /// slice compositor the live pipeline uses. What is written is the picture
    /// the viewer was handed, at the instants the dump recorded.
    ///
    /// A take that begins in the middle of a prediction chain has no earlier
    /// picture to predict from, so frames before the first sync frame are absent
    /// from the rebuild exactly as they were undecodable live.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn rebuild_media_stream_dump(
        index: &ard_rs::RawStreamIndex,
        index_path: &Path,
        output: &Path,
        bitrate: u32,
    ) -> Result<RebuildSummary, String> {
        use crate::media::pipeline::{
            DecoderOutputAssembler, PendingFrameTiming, platform_decoder,
        };
        use ard_rs::media_stream::{RtpPacket, VideoStreamAssembler};

        if index.records.is_empty() {
            return Err("裸流没有任何记录".into());
        }
        if index
            .records
            .iter()
            .any(|record| record.framing != ard_rs::RecordFraming::UdpPacket)
        {
            return Err(format!("{} 不是媒体流裸流", index_path.display()));
        }
        let Some(codec) = codec_from_header(&index.header) else {
            return Err(format!(
                "裸流头缺少编解码器；无法重建：{}",
                index.header.trim()
            ));
        };
        let Some((width, height)) =
            dimensions_from_header(&index.header).or_else(read_size_override)
        else {
            return Err(
                "裸流头没有记录帧尺寸，且未设置 ARD_RAW_STREAM_SIZE=宽x高；无法重建".to_owned(),
            );
        };
        let raw = std::fs::read(ard_rs::RawStreamIndex::raw_path(index_path))
            .map_err(|error| format!("无法读取裸流：{error}"))?;

        // Bands are addressed by SSRC and ordered by it, which is the mapping the
        // live receiver uses (`base_remote_ssrc + layer`).
        let ssrcos: Vec<u32> = index
            .records
            .iter()
            .filter_map(|record| record.ssrc)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        if ssrcos.is_empty() {
            return Err("媒体流裸流没有带 SSRC 的数据包".into());
        }
        let mut assembler = VideoStreamAssembler::new(codec);
        for ssrc in &ssrcos {
            assembler.expect_stream(*ssrc);
        }

        let mut decoder = platform_decoder(codec);
        let mut outputs = DecoderOutputAssembler::new((width, height), Some((width, height)));
        // The dump's clock becomes the decode timeline's clock, so a frame's
        // presentation time is its arrival on the recording's timeline.
        let clock = std::time::Instant::now();
        let base_ms = index.records.first().map(|record| record.t).unwrap_or(0);

        let mut encoder = encoder::H264Encoder::new(encoder::EncoderSettings {
            width,
            height,
            bitrate,
            frame_rate_hint: FRAME_RATE_HINT,
            primaries: crate::media::YuvPrimaries::Bt709,
        })?;
        let mut muxer = muxer::Mp4Muxer::create(output)?;
        let segment = RebuildSegment { width, height };
        let mut samples = Vec::new();
        // The decoded bands are composed into one persistent NV12 frame, which
        // is exactly what the renderer's two-plane textures hold live.
        let mut composed_y = vec![0_u8; (width * height) as usize];
        let mut composed_uv = vec![0_u8; (width * height.div_ceil(2)) as usize];
        let mut frames = 0_u64;
        let mut reassembled = 0_u64;

        for (number, record) in index.records.iter().enumerate() {
            let start = record.offset as usize;
            let end = start
                .checked_add(record.length as usize)
                .ok_or("记录超出裸流范围")?;
            let packet = raw
                .get(start..end)
                .ok_or_else(|| format!("记录 {number} 超出裸流范围"))?;
            let header = RtpPacket::parse(packet)
                .map_err(|error| format!("记录 {number} 不是 RTP 包：{error}"))?
                .header;
            let arrived = clock + Duration::from_millis(record.t.saturating_sub(base_ms));
            assembler
                .push_packet(header.ssrc, packet, arrived)
                .map_err(|error| format!("记录 {number} 无法重组：{error}"))?;
            let assembled = assembler
                .receive()
                .map_err(|error| format!("记录 {number} 无法成帧：{error}"))?;
            let Some(frame) = assembled.frame else {
                continue;
            };
            reassembled += 1;
            let access_units = frame.access_units.len();
            outputs.register_batch(
                frame.timestamp,
                access_units,
                PendingFrameTiming {
                    first_packet_received_at: frame.first_packet_received_at,
                    first_access_unit_completed_at: frame.first_access_unit_completed_at,
                    batch_released_at: frame.first_access_unit_completed_at,
                },
            )?;
            let mut decoded = Vec::new();
            for (slice_index, unit) in frame.access_units {
                decoded.extend(decoder.decode(slice_index, &unit));
            }
            decoded.extend(decoder.finish_frame());
            if let Some(error) = decoder.take_errors().into_iter().next() {
                return Err(format!("视频解码失败：{error}"));
            }
            for picture in outputs.push(decoded)? {
                let pts = picture
                    .timing
                    .map(|timing| {
                        timing
                            .first_packet_received_at
                            .saturating_duration_since(clock)
                    })
                    .unwrap_or_default();
                compose_nv12(&picture, &mut composed_y, &mut composed_uv, width, height)?;
                encoder.push(
                    encoder::SourceFrame::Nv12 {
                        y_stride: width as usize,
                        y: &composed_y,
                        uv_stride: width as usize,
                        uv: &composed_uv,
                        range: picture.range,
                        matrix: picture.matrix,
                    },
                    pts,
                )?;
                encoder.take_samples(&mut samples);
                write_rebuild_samples(&mut muxer, &encoder, &mut samples, &segment)?;
                frames += 1;
            }
        }

        // Drain whatever the decoder still holds, then the encoder's tail.
        let drained = outputs.push(decoder.finish_frame())?;
        for picture in drained {
            compose_nv12(&picture, &mut composed_y, &mut composed_uv, width, height)?;
            let pts = picture
                .timing
                .map(|timing| {
                    timing
                        .first_packet_received_at
                        .saturating_duration_since(clock)
                })
                .unwrap_or_default();
            encoder.push(
                encoder::SourceFrame::Nv12 {
                    y_stride: width as usize,
                    y: &composed_y,
                    uv_stride: width as usize,
                    uv: &composed_uv,
                    range: picture.range,
                    matrix: picture.matrix,
                },
                pts,
            )?;
            encoder.take_samples(&mut samples);
            write_rebuild_samples(&mut muxer, &encoder, &mut samples, &segment)?;
            frames += 1;
        }
        if frames == 0 {
            return Err(format!(
                "媒体流裸流没有解出可显示的画面（重组 {reassembled} 帧）"
            ));
        }
        let end = index
            .records
            .last()
            .map(|record| record.t.saturating_sub(base_ms))
            .unwrap_or_default();
        encoder.finish(Duration::from_millis(end))?;
        encoder.take_samples(&mut samples);
        write_rebuild_samples(&mut muxer, &encoder, &mut samples, &segment)?;
        let summary = muxer.finalize()?;
        Ok(RebuildSummary {
            frames,
            bytes: summary.bytes,
        })
    }

    /// The codec a media dump header names.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn codec_from_header(header: &str) -> Option<ard_rs::media_stream::MediaStreamCodec> {
        use ard_rs::media_stream::MediaStreamCodec;
        let value = header.split_once("\"codec\":\"")?.1;
        let name = value.split('"').next()?.to_ascii_uppercase();
        if name.contains("265") || name.contains("HEVC") {
            Some(MediaStreamCodec::Hevc)
        } else if name.contains("264") || name.contains("AVC") {
            Some(MediaStreamCodec::H264)
        } else {
            None
        }
    }

    /// The frame size a media dump needs: the header's, or the explicit
    /// `ARD_RAW_STREAM_SIZE=WxH` fallback for a dump written before the header
    /// carried it.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn read_size_override() -> Option<(u32, u32)> {
        let value = std::env::var("ARD_RAW_STREAM_SIZE").ok()?;
        let (width, height) = value.trim().split_once(['x', 'X'])?;
        Some((width.trim().parse().ok()?, height.trim().parse().ok()?))
    }

    /// The frame size a media dump header records.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn dimensions_from_header(header: &str) -> Option<(u32, u32)> {
        let number = |key: &str| -> Option<u32> {
            let rest = header.split_once(&format!("\"{key}\":"))?.1;
            rest.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .ok()
        };
        Some((number("width")?, number("height")?))
    }

    /// Write one decoded picture into the persistent full-frame NV12 buffers.
    ///
    /// The compositor hands back only the bands that changed, positioned in the
    /// frame, which is what the live renderer uploads into its textures. The
    /// encoder takes a whole frame, so the bands are laid into one.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn compose_nv12(
        picture: &crate::media::DecodedFrame,
        y: &mut [u8],
        uv: &mut [u8],
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        let width = width as usize;
        let uv_width = width;
        let uv_height = (height as usize).div_ceil(2);
        for update in &picture.updates {
            let source = &update.pixels;
            let source_width = source.width as usize;
            for row in 0..update.y_rows as usize {
                let from = row * source_width;
                let to = (update.y_origin as usize + row) * width;
                let Some(source_row) = source.y_plane.get(from..from + width) else {
                    return Err("解码分片的亮度平面短于帧宽".into());
                };
                let Some(target_row) = y.get_mut(to..to + width) else {
                    return Err("解码分片的亮度行超出帧缓冲".into());
                };
                target_row.copy_from_slice(source_row);
            }
            for row in 0..update.uv_rows as usize {
                let from = row * source_width;
                let to = (update.uv_origin as usize + row) * uv_width;
                let Some(source_row) = source.uv_plane.get(from..from + uv_width) else {
                    return Err("解码分片的色度平面短于帧宽".into());
                };
                let Some(target_row) = uv.get_mut(to..to + uv_width) else {
                    return Err("解码分片的色度行超出帧缓冲".into());
                };
                target_row.copy_from_slice(source_row);
            }
        }
        if uv.len() != uv_width * uv_height {
            return Err("帧缓冲的色度平面尺寸与分辨率不一致".into());
        }
        Ok(())
    }

    /// Write the encoder's finished access units into a segment's muxer.
    ///
    /// The parameter sets arrive with the first sample the encoder releases, and
    /// nothing before that can be described by an avcC configuration box, so a
    /// segment that has not reached its first keyframe yet writes nothing. The
    /// tail flush always reaches one, which is why the last call cannot drop
    /// samples.
    fn write_rebuild_samples(
        muxer: &mut muxer::Mp4Muxer,
        encoder: &encoder::H264Encoder,
        samples: &mut Vec<encoder::EncodedSample>,
        segment: &RebuildSegment,
    ) -> Result<(), String> {
        let Some(sets) = encoder.parameter_sets() else {
            samples.clear();
            return Ok(());
        };
        for sample in samples.drain(..) {
            muxer.write_sample(
                &muxer::VideoTrackFormat {
                    width: segment.width,
                    height: segment.height,
                    sps: sets.sps.clone(),
                    pps: sets.pps.clone(),
                },
                sample,
            )?;
        }
        Ok(())
    }

    /// A dump of a record stream has to rebuild into the same frames the
    /// session presented, in the same order and at the same times, or comparing
    /// the rebuild with the recording would prove nothing.
    #[test]
    fn a_record_stream_dump_rebuilds_into_the_frames_it_captured() {
        if !encoder::supported() {
            eprintln!("skipping: this platform has no system H.264 encoder");
            return;
        }
        let directory = std::env::temp_dir().join(format!(
            "ard-rebuild-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&directory).expect("temp directory");

        // Three raw-encoded frames with very distinct pixels, on the take's
        // clock. A rate-controlled encoder drops a frame it considers a
        // duplicate, so the patterns have to differ everywhere, not only in one
        // patch.
        let width = 32_u32;
        let height = 24_u32;
        let frames: Vec<Vec<u8>> = (0..3_u32)
            .map(|index| {
                let mut frame = vec![0_u8; (width * height * 4) as usize];
                for y in 0..height {
                    for x in 0..width {
                        let offset = ((y * width + x) * 4) as usize;
                        frame[offset] = (x * 8 + index * 85) as u8;
                        frame[offset + 1] = (y * 10 + index * 60) as u8;
                        frame[offset + 2] = (index * 90) as u8;
                        frame[offset + 3] = 255;
                    }
                }
                frame
            })
            .collect();
        let times = [0_u64, 40, 2_000];
        let raw_path = directory.join("host server stream.raw");
        let index_path = directory.join("host server stream.jsonl");
        let mut raw = Vec::new();
        let mut index = String::from(
            "{\"ard_raw_stream\":2,\"source\":\"server\",\"stream\":\"server\",\"content\":\"decrypted\"}\n",
        );
        // The take starts with the size the session had, the way a live record
        // stream does.
        let mut resize = vec![0_u8, 0, 0, 1];
        resize.extend_from_slice(&0_u16.to_be_bytes());
        resize.extend_from_slice(&0_u16.to_be_bytes());
        resize.extend_from_slice(&(width as u16).to_be_bytes());
        resize.extend_from_slice(&(height as u16).to_be_bytes());
        resize.extend_from_slice(&(-223_i32).to_be_bytes());
        raw.extend_from_slice(&resize);
        index.push_str(&format!(
            "{{\"sequence\":0,\"offset\":0,\"length\":{},\"framed\":\"tcp-record-plaintext\",\"t\":0}}\n",
            resize.len()
        ));
        for (number, frame) in frames.iter().enumerate() {
            let number = number + 1;
            // A Raw rectangle carries the pixels in the RFB byte order the
            // viewer requested, which for XRGB8888 is little-endian BGRX.
            let mut record = vec![0_u8, 0, 0, 1];
            record.extend_from_slice(&0_u16.to_be_bytes());
            record.extend_from_slice(&0_u16.to_be_bytes());
            record.extend_from_slice(&(width as u16).to_be_bytes());
            record.extend_from_slice(&(height as u16).to_be_bytes());
            record.extend_from_slice(&0_i32.to_be_bytes());
            for pixel in frame.chunks_exact(4) {
                record.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 0]);
            }
            let offset = raw.len();
            raw.extend_from_slice(&record);
            index.push_str(&format!(
                "{{\"sequence\":{number},\"offset\":{offset},\"length\":{},\"framed\":\"tcp-record-plaintext\",\"t\":{}}}\n",
                record.len(),
                times[number - 1]
            ));
        }
        index.push_str(&format!(
            "{{\"end\":true,\"records\":{},\"bytes\":{},\"take_ms\":{}}}\n",
            frames.len() + 1,
            raw.len(),
            times[times.len() - 1]
        ));
        std::fs::write(&raw_path, &raw).expect("raw written");
        std::fs::write(&index_path, &index).expect("index written");

        let output = directory.join("rebuilt.mp4");
        let parsed = ard_rs::RawStreamIndex::read(&index_path).expect("index readable");
        let summary =
            rebuild_record_stream(&parsed, &index_path, &output, 8_000_000).expect("rebuild runs");
        assert_eq!(summary.frames, frames.len() as u64);
        assert!(summary.bytes > 0);
        assert!(output.exists());

        // The rebuilt video covers exactly the dumped frames on exactly the
        // dumped timeline: the same record the recorder's own test uses, so the
        // two are directly comparable.
        let Some(probe) = run_tool(
            "ffprobe",
            &[
                "-v",
                "error",
                "-print_format",
                "json",
                "-show_packets",
                output.to_str().expect("utf-8 path"),
            ],
        ) else {
            eprintln!("skipping the timeline check: ffprobe is not installed");
            std::fs::remove_dir_all(&directory).ok();
            return;
        };
        let probe: serde_json::Value = serde_json::from_slice(&probe).expect("ffprobe emits json");
        let packets = probe["packets"].as_array().expect("packet list");
        assert_eq!(
            packets.len(),
            frames.len(),
            "one video frame per dumped frame"
        );
        // The frame's own clock is the time axis: the second frame was dumped
        // 40 ms into the take and the third 2 s in, and the container has to say
        // the same.
        for (index, packet) in packets.iter().enumerate() {
            let seconds = packet["pts_time"]
                .as_str()
                .expect("pts")
                .parse::<f64>()
                .expect("numeric pts");
            let expected = times[index] as f64 / 1000.0;
            assert!(
                (seconds - expected).abs() < 0.001,
                "frame {index} is at {seconds}s, the dump says {expected}s"
            );
        }

        std::fs::remove_dir_all(&directory).ok();
    }

    /// A media-stream dump has to rebuild into the same frames the session
    /// decoded, in the same order and at the same times, or comparing the rebuild
    /// with the recording would prove nothing.
    ///
    /// The input is the oracle's real four-band H.264 elementary stream,
    /// packetized the way the server packetizes it: four adjacent SSRCs, one RTP
    /// timestamp per desktop frame, and one global DON per access unit.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn a_media_stream_dump_rebuilds_into_the_frames_it_captured() {
        if !encoder::supported() {
            eprintln!("skipping: this platform has no system H.264 encoder");
            return;
        }
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../ard-core/examples/fixtures/oracle-diagonal-frames-1920x1080-4x272.h264");
        let Ok(bytes) = std::fs::read(&fixture) else {
            eprintln!("skipping: {} is not readable", fixture.display());
            return;
        };
        let access_units = annex_b_access_units(&bytes);
        assert!(
            access_units.len() >= 8,
            "the fixture must hold at least two desktop frames"
        );

        let directory = std::env::temp_dir().join(format!(
            "ard-media-rebuild-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&directory).expect("temp directory");
        let index_path = directory.join("host video stream.jsonl");
        let (raw, index) = packetize_media_dump(&access_units);
        std::fs::write(directory.join("host video stream.raw"), &raw).expect("raw written");
        std::fs::write(&index_path, &index).expect("index written");

        let parsed = ard_rs::RawStreamIndex::read(&index_path).expect("index readable");
        let output = directory.join("rebuilt.mp4");
        let summary = rebuild_media_stream_dump(&parsed, &index_path, &output, 20_000_000)
            .expect("the media dump rebuilds");

        // The fixture holds 300 desktop frames; a rebuild that decodes them all
        // proves the reassembly and the decode chain, not just the first frame.
        assert!(
            summary.frames >= 250,
            "rebuilt {} of 300 desktop frames",
            summary.frames
        );
        assert!(summary.bytes > 0);

        let Some(probe) = run_tool(
            "ffprobe",
            &[
                "-v",
                "error",
                "-print_format",
                "json",
                "-show_packets",
                output.to_str().expect("utf-8 path"),
            ],
        ) else {
            eprintln!("skipping the timeline check: ffprobe is not installed");
            std::fs::remove_dir_all(&directory).ok();
            return;
        };
        let probe: serde_json::Value = serde_json::from_slice(&probe).expect("ffprobe emits json");
        let packets = probe["packets"].as_array().expect("packet list");
        assert_eq!(
            packets.len() as u64,
            summary.frames,
            "the video holds the frames the rebuild wrote"
        );
        // The dump's clock is the video's clock: the first desktop frame is at
        // zero and the frames advance at the 60 Hz cadence the dump recorded.
        let first = packets[0]["pts_time"]
            .as_str()
            .expect("pts")
            .parse::<f64>()
            .expect("numeric pts");
        assert!(first.abs() < 0.001, "the first frame is at {first}s");
        for (number, packet) in packets.iter().enumerate().take(16).skip(1) {
            let seconds = packet["pts_time"]
                .as_str()
                .expect("pts")
                .parse::<f64>()
                .expect("numeric pts");
            // The dump stamps frames at the 60 Hz cadence, rounded to the
            // millisecond the index records.
            let expected = (number as u64 * 1000 / 60) as f64 / 1000.0;
            assert!(
                (seconds - expected).abs() < 0.002,
                "frame {number} is at {seconds}s, the dump says {expected}s"
            );
        }

        // The picture has to be the fixture's, not merely a frame of the right
        // size: the top band of the rebuilt frame is compared with the fixture's
        // first band decoded independently, which is the same check the live
        // compositor passes.
        let Some(reference) = run_tool(
            "ffmpeg",
            &[
                "-v",
                "error",
                "-i",
                fixture.to_str().expect("utf-8 path"),
                "-frames:v",
                "1",
                "-pix_fmt",
                "yuv420p",
                "-f",
                "rawvideo",
                "-",
            ],
        ) else {
            eprintln!("skipping the pixel check: ffmpeg is not installed");
            std::fs::remove_dir_all(&directory).ok();
            return;
        };
        let rebuilt = run_tool(
            "ffmpeg",
            &[
                "-v",
                "error",
                "-i",
                output.to_str().expect("utf-8 path"),
                "-frames:v",
                "1",
                "-pix_fmt",
                "yuv420p",
                "-f",
                "rawvideo",
                "-",
            ],
        )
        .expect("ffmpeg decodes the rebuild");
        const BAND_ROWS: usize = 272;
        const WIDTH: usize = 1920;
        assert!(reference.len() >= WIDTH * BAND_ROWS);
        assert!(rebuilt.len() >= WIDTH * 1080 * 3 / 2);
        let difference: u64 = reference[..WIDTH * BAND_ROWS]
            .iter()
            .zip(rebuilt[..WIDTH * BAND_ROWS].iter())
            .map(|(left, right)| u64::from(left.abs_diff(*right)))
            .sum();
        let mean = difference as f64 / (WIDTH * BAND_ROWS) as f64;
        eprintln!("rebuilt band vs fixture: mean |Δ| {mean:.3} luma levels");
        assert!(
            mean < 6.0,
            "the rebuilt band differs from the fixture by {mean:.2} luma levels on average"
        );

        std::fs::remove_dir_all(&directory).ok();
    }

    /// Split an Annex-B elementary stream into access units at its AUD NALs.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn annex_b_access_units(bytes: &[u8]) -> Vec<Vec<Vec<u8>>> {
        // Every NAL unit begins with a three- or four-byte start code.
        let mut nals: Vec<Vec<u8>> = Vec::new();
        let mut position = 0_usize;
        let mut start = None;
        while position + 3 <= bytes.len() {
            let three = bytes[position..position + 3] == [0, 0, 1];
            let four = position + 4 <= bytes.len() && bytes[position..position + 4] == [0, 0, 0, 1];
            if four || three {
                if let Some(previous) = start {
                    let end = position;
                    if end > previous {
                        nals.push(bytes[previous..end].to_vec());
                    }
                }
                position += if four { 4 } else { 3 };
                start = Some(position);
                continue;
            }
            position += 1;
        }
        if let Some(previous) = start
            && previous < bytes.len()
        {
            nals.push(bytes[previous..].to_vec());
        }

        let mut units: Vec<Vec<Vec<u8>>> = Vec::new();
        for nal in nals {
            let unit = units
                .last_mut()
                .filter(|_| nal.first().is_some_and(|byte| byte & 0x1f != 9));
            match unit {
                Some(unit) => unit.push(nal),
                None => units.push(vec![nal]),
            }
        }
        // An AUD-only first unit (the fixture opens with one) carries no picture.
        units.retain(|unit| {
            unit.iter()
                .any(|nal| nal.first().is_some_and(|byte| matches!(byte & 0x1f, 1..=5)))
        });
        units
    }

    /// Packetize access units the way Apple does, and return a dump of them.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn packetize_media_dump(access_units: &[Vec<Vec<u8>>]) -> (Vec<u8>, String) {
        /// Bytes of NAL body per FU-B packet. Apple's packets are around this
        /// size; the exact split does not matter, only that fragments reassemble.
        const FRAGMENT: usize = 1100;
        const BASE_SSRC: u32 = 0x1d00_0000;
        const BANDS: usize = 4;
        const FRAME_RATE: u32 = 60;
        // 90 kHz is the RTP video clock; the live stream advances one frame per
        // 60 Hz tick.
        const TICKS_PER_FRAME: u32 = 90_000 / FRAME_RATE;

        let mut raw = Vec::new();
        let mut index = String::from(
            "{\"ard_raw_stream\":2,\"source\":\"server\",\"stream\":\"video\",\"content\":\"decrypted\",\"codec\":\"H264\",\"payload_type\":96,\"width\":1920,\"height\":1080}\n",
        );
        let mut sequence = [1000_u16; BANDS];
        let mut entries = 0_usize;

        for (number, unit) in access_units.iter().enumerate() {
            let band = number % BANDS;
            let frame = number / BANDS;
            let ssrc = BASE_SSRC + band as u32;
            let timestamp = frame as u32 * TICKS_PER_FRAME;
            let don = (number as u16).wrapping_add(1);
            let arrival_ms = frame as u64 * 1000 / u64::from(FRAME_RATE);
            // Every NAL travels as FU-B fragments, which is how the DON the
            // cross-band ordering needs reaches the receiver.
            let mut packets: Vec<Vec<u8>> = Vec::new();
            for (nal_number, nal) in unit.iter().enumerate() {
                let nal_type = nal[0] & 0x1f;
                let nal_ref_idc = nal[0] & 0x60;
                let body = &nal[1..];
                let last_nal = nal_number + 1 == unit.len();
                let chunks: Vec<&[u8]> = if body.is_empty() {
                    vec![&[]]
                } else {
                    body.chunks(FRAGMENT).collect()
                };
                for (chunk_number, chunk) in chunks.iter().enumerate() {
                    let start = chunk_number == 0;
                    let end = chunk_number + 1 == chunks.len();
                    let mut payload = vec![
                        nal_ref_idc | 29,
                        (u8::from(start) << 7) | (u8::from(end) << 6) | nal_type,
                        (don >> 8) as u8,
                        don as u8,
                    ];
                    payload.extend_from_slice(chunk);
                    packets.push(payload);
                }
                let _ = last_nal;
            }
            let packet_count = packets.len();
            for (position, payload) in packets.into_iter().enumerate() {
                let marker = position + 1 == packet_count;
                let sequence = &mut sequence[band];
                // The marker bit ends the access unit, so it is 0x80, not 1.
                let mut packet = vec![0x80, 0x60 | if marker { 0x80 } else { 0 }];
                packet.extend_from_slice(&sequence.to_be_bytes());
                packet.extend_from_slice(&timestamp.to_be_bytes());
                packet.extend_from_slice(&ssrc.to_be_bytes());
                packet.extend_from_slice(&payload);
                *sequence = sequence.wrapping_add(1);

                let offset = raw.len();
                raw.extend_from_slice(&packet);
                index.push_str(&format!(
                    "{{\"sequence\":{entries},\"offset\":{offset},\"length\":{},\"framed\":\"rtp-plaintext\",\"t\":{arrival_ms},\"rtp\":{},\"ssrc\":{ssrc}}}\n",
                    packet.len(),
                    u16::from_be_bytes([packet[2], packet[3]]),
                ));
                entries += 1;
            }
        }
        let bytes = raw.len();
        let take_ms = (access_units.len() / BANDS) as u64 * 1000 / u64::from(FRAME_RATE);
        index.push_str(&format!(
            "{{\"end\":true,\"records\":{entries},\"bytes\":{bytes},\"take_ms\":{take_ms}}}\n"
        ));
        (raw, index)
    }

    #[test]
    fn quality_targets_scale_with_the_recorded_frame_size() {
        let small = RecordingQuality::Balanced.bitrate(640, 360);
        let large = RecordingQuality::Balanced.bitrate(2880, 1800);
        assert!(small >= 1_500_000);
        assert!(large > small);
        assert!(RecordingQuality::High.bitrate(2880, 1800) > large);
        assert!(RecordingQuality::Compact.bitrate(2880, 1800) < large);
        assert!(RecordingQuality::High.bitrate(4096, 4096) <= 120_000_000);
    }

    #[test]
    fn frame_layouts_describe_their_staging_regions() {
        let rgba = FrameLayout::bgra(1920, 1080);
        assert_eq!(rgba.width(), 1920);
        assert_eq!(rgba.height(), 1080);
        assert!(!rgba.is_nv12());
        assert_eq!(rgba.buffer_size(), 1920 * 4 * 1080);

        // Both planes are padded to the 256-byte copy alignment: a 1440-pixel
        // luma row occupies 1536 bytes, and the chroma plane starts on the next
        // aligned offset.
        let nv12 = FrameLayout::nv12(
            1440,
            900,
            YuvRange::Video,
            YuvMatrix::Bt709,
            YuvPrimaries::Bt709,
        );
        assert!(nv12.is_nv12());
        assert_eq!(nv12.buffer_size(), 1536 * 900 + 1536 * 450);
    }
}
