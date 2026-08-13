use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{IpAddr, TcpStream};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::media_stream::{
    CLIENT_MEDIA_STREAM_MESSAGE_TYPE, ENCODING_AVC_MEDIA_STREAM, MEDIA_STREAM_MESSAGE_VERSION,
    MediaStreamAnswer, MediaStreamCodec, MediaStreamConfiguration, MediaStreamFlags,
    MediaStreamKeyMaterial, MediaStreamOffer, MediaStreamServerReply, MediaUdpEndpoints,
    MediaUdpPortOverrides, VideoCodecConfig, build_media_stream_offer_with_ssrc,
    build_media_stream_offer_with_ssrc_and_codec, build_remote_endpoint_info,
};
use crate::{
    ArdDisplayConfiguration, ArdEncryptionControl, ArdMessageDispatcher, ArdScrollWheelEvent,
    ArdServerMessage, ArdVerifiedRecordStream, ArdViewerInformation, Decoder, Framebuffer,
    FramebufferFormat, PixelFormat, ProtocolVersion, SecurityType, build_ard_auto_frame_update,
    build_ard_encryption_activation, build_ard_scroll_wheel_event,
    build_ard_set_display_configuration, build_ard_set_encryption_level,
    build_ard_type30_client_exchange, build_client_cut_text, build_framebuffer_update_request,
    build_key_event, build_pointer_event, build_set_encodings, build_set_pixel_format,
    parse_ard_auth_challenge, parse_framebuffer_update, parse_security_types, parse_server_init,
    unwrap_ard_session_material,
};

const MAX_KEY_BYTES: usize = 512;
const MAX_RECORD_BYTES: usize = u16::MAX as usize;
const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_CUT_TEXT_BYTES: usize = 1024 * 1024;
const MAX_SERVER_NAME_BYTES: usize = 1024 * 1024;
const MAX_INPUT_QUEUE: usize = 512;
const MAX_OUTBOUND_PAYLOAD_BYTES: usize = 65_498;

fn generate_media_ssrc() -> Result<u32, ArdClientError> {
    loop {
        let mut bytes = [0_u8; 4];
        getrandom::fill(&mut bytes).map_err(|error| {
            ArdClientError::Message(format!("AVC SSRC random source failed: {error}"))
        })?;
        let ssrc = u32::from_be_bytes(bytes);
        if ssrc != 0 {
            return Ok(ssrc);
        }
    }
}

/// ARD image-quality profiles, matching the encoding families exposed by
/// Apple Screen Sharing and Remote Desktop Manager.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ArdVideoQuality {
    Low,
    Medium,
    High,
    /// Apple AVC media stream constrained to HEVC over UDP/SRTP.
    HighPerformanceHevc,
    /// Apple AVC media stream constrained to H.264/AVC over UDP/SRTP.
    HighPerformanceAvc,
    #[default]
    Adaptive,
    Full,
}

/// Selects the RFB pixel representation exposed by [`ArdClient::framebuffer`].
/// The core stores this representation as-is; presentation and texture
/// conversion are outside the core package.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ArdFrameOutput {
    /// Request and retain a caller-selected RFB pixel layout. The core does
    /// not convert the resulting bytes to a presentation or texture format.
    Native(PixelFormat),
    /// Keep the server's advertised RFB pixel layout.
    #[default]
    ServerNative,
}

impl ArdFrameOutput {
    fn pixel_format(self, server_native: PixelFormat) -> PixelFormat {
        match self {
            Self::Native(pixel_format) => pixel_format,
            Self::ServerNative => server_native,
        }
    }

    fn framebuffer_format(self, server_native: PixelFormat) -> FramebufferFormat {
        match self {
            Self::Native(pixel_format) => FramebufferFormat::Native(pixel_format),
            Self::ServerNative => FramebufferFormat::Native(server_native),
        }
    }
}

/// Reconnection policy used by [`ArdClient::next_event`]. A zero-attempt
/// policy keeps the historical fail-fast behavior while callers that need a
/// long-lived session can opt into bounded automatic reconnects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArdReconnectPolicy {
    pub max_attempts: usize,
    pub delay: Duration,
}

impl ArdReconnectPolicy {
    pub const fn disabled() -> Self {
        Self {
            max_attempts: 0,
            delay: Duration::ZERO,
        }
    }

    pub const fn new(max_attempts: usize, delay: Duration) -> Self {
        Self {
            max_attempts,
            delay,
        }
    }
}

impl Default for ArdReconnectPolicy {
    fn default() -> Self {
        Self::disabled()
    }
}

impl ArdVideoQuality {
    pub fn encodings(self) -> &'static [i32] {
        match self {
            Self::Low => &[1000, 6, 16, -223],
            Self::Medium => &[1001, 6, 16, -223],
            Self::High => &[1002, 6, 16, -223],
            Self::HighPerformanceHevc | Self::HighPerformanceAvc => {
                // High-performance is an explicit transport contract. Do not
                // silently negotiate MVS/zlib/raw when AVC media setup fails;
                // callers must receive a visible negotiation failure instead.
                &[ENCODING_AVC_MEDIA_STREAM, -223]
            }
            Self::Adaptive => &[1011, 1002, 6, 16, -223],
            Self::Full => &[6, 16, -223],
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Low => "黑白",
            Self::Medium => "灰度",
            Self::High => "16位颜色",
            Self::HighPerformanceHevc => "HEVC (H.265)",
            Self::HighPerformanceAvc => "AVC (H.264)",
            Self::Adaptive => "自适应 MVS",
            Self::Full => "全色",
        }
    }

    pub const fn is_high_performance(self) -> bool {
        matches!(self, Self::HighPerformanceHevc | Self::HighPerformanceAvc)
    }

    const fn preferred_media_codec(self) -> Option<MediaStreamCodec> {
        match self {
            Self::HighPerformanceHevc => Some(MediaStreamCodec::Hevc),
            Self::HighPerformanceAvc => Some(MediaStreamCodec::H264),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub struct ArdClientConfig {
    pub address: String,
    pub username: Vec<u8>,
    pub password: Vec<u8>,
    pub timeout: Duration,
    pub video_quality: ArdVideoQuality,
    /// Optional fixed virtual-display layout requested from the server.
    /// `None` keeps the server's existing physical display layout.
    pub display_configuration: Option<ArdDisplayConfiguration>,
    /// Optional external UDP destinations for a remote Mac behind explicit
    /// port forwarding. Empty fields keep the ports negotiated over RFB.
    pub media_udp_port_overrides: MediaUdpPortOverrides,
    /// RFB pixel layout requested from the server and retained by the core.
    pub output_format: ArdFrameOutput,
    /// Use Apple's server-driven update stream instead of serial
    /// request/response polling.
    pub automatic_updates: bool,
    /// Minimum interval between automatic updates. Zero is the native default
    /// and permits the server's maximum supported rate.
    pub frame_interval: Duration,
    pub reconnect: ArdReconnectPolicy,
}

impl fmt::Debug for ArdClientConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ArdClientConfig")
            .field("address", &self.address)
            .field("username_len", &self.username.len())
            .field("password", &"<redacted>")
            .field("timeout", &self.timeout)
            .field("video_quality", &self.video_quality)
            .field("display_configuration", &self.display_configuration)
            .field("media_udp_port_overrides", &self.media_udp_port_overrides)
            .field("output_format", &self.output_format)
            .field("automatic_updates", &self.automatic_updates)
            .field("frame_interval", &self.frame_interval)
            .field("reconnect", &self.reconnect)
            .finish()
    }
}

impl ArdClientConfig {
    pub fn new(
        address: impl Into<String>,
        username: impl Into<Vec<u8>>,
        password: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            address: address.into(),
            username: username.into(),
            password: password.into(),
            timeout: Duration::from_secs(20),
            video_quality: ArdVideoQuality::Adaptive,
            display_configuration: None,
            media_udp_port_overrides: MediaUdpPortOverrides::default(),
            output_format: ArdFrameOutput::ServerNative,
            automatic_updates: true,
            frame_interval: Duration::ZERO,
            reconnect: ArdReconnectPolicy::default(),
        }
    }
}

impl Drop for ArdClientConfig {
    fn drop(&mut self) {
        self.password.fill(0);
    }
}

#[derive(Debug)]
pub enum ArdClientError {
    Io(io::Error),
    Protocol(crate::Error),
    Message(String),
}

impl fmt::Display for ArdClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::Protocol(error) => write!(formatter, "{error}"),
            Self::Message(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ArdClientError {}

impl From<io::Error> for ArdClientError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<crate::Error> for ArdClientError {
    fn from(error: crate::Error) -> Self {
        Self::Protocol(error)
    }
}

impl ArdClientError {
    fn is_io(&self) -> bool {
        matches!(self, Self::Io(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArdFrameInfo {
    pub index: u64,
    pub framebuffer_updates: usize,
    pub rectangle_count: usize,
    pub payload_bytes: usize,
    /// Actual encrypted server-to-client bytes, including each record's
    /// two-byte length prefix and block padding.
    pub wire_bytes: usize,
}

/// An event delivered by a connected ARD session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArdClientEvent {
    Frame(ArdFrameInfo),
    Clipboard(String),
    Bell,
    StateChange,
    /// The server accepted the AVC media path and the viewer can start its
    /// UDP/SRTP decoder with the supplied video1 material.
    MediaStream(Box<ArdMediaStream>),
    /// The transport was recreated after a read-side disconnect. The next
    /// call waits for the first frame from the new session.
    Reconnected,
}

/// Negotiated server-to-viewer AVC video stream parameters.
#[derive(Clone, PartialEq, Eq)]
pub struct ArdMediaStream {
    pub endpoints: MediaUdpEndpoints,
    /// Server-to-viewer SRTP master key and salt.
    pub key_blob: Vec<u8>,
    /// Viewer-to-server SRTCP master key and salt used for feedback.
    pub feedback_key_blob: Vec<u8>,
    pub codec: MediaStreamCodec,
    pub payload_type: u8,
    pub codec_config: VideoCodecConfig,
    /// The server's negotiated video SSRC from the answer. Cipher suite 5
    /// uses it when constructing the SRTP counter block.
    pub derived_ssrc: u32,
    /// The viewer SSRC advertised in the offer and used by outbound SRTCP.
    pub local_ssrc: u32,
}

impl ArdMediaStream {
    /// Move only the values needed by the video worker. The negotiated key is
    /// removed from `self` before its `Drop` implementation runs so the
    /// event does not leave a second live copy while the worker is starting.
    pub fn into_video_pipeline_parts(
        mut self,
    ) -> (
        MediaUdpEndpoints,
        Vec<u8>,
        Vec<u8>,
        MediaStreamCodec,
        u8,
        u32,
        u32,
    ) {
        let key_blob = core::mem::take(&mut self.key_blob);
        let feedback_key_blob = core::mem::take(&mut self.feedback_key_blob);
        (
            self.endpoints,
            key_blob,
            feedback_key_blob,
            self.codec,
            self.payload_type,
            self.derived_ssrc,
            self.local_ssrc,
        )
    }

    /// Move the complete negotiated video configuration into the formal
    /// receive pipeline. The codec and RTP payload are deliberately taken
    /// from the answer object instead of being reconstructed from decrypted
    /// packet bytes.
    pub fn into_video_pipeline_parts_with_config(
        mut self,
    ) -> (
        MediaUdpEndpoints,
        Vec<u8>,
        Vec<u8>,
        VideoCodecConfig,
        u32,
        u32,
    ) {
        let key_blob = core::mem::take(&mut self.key_blob);
        let feedback_key_blob = core::mem::take(&mut self.feedback_key_blob);
        let codec_config = core::mem::take(&mut self.codec_config);
        (
            self.endpoints,
            key_blob,
            feedback_key_blob,
            codec_config,
            self.derived_ssrc,
            self.local_ssrc,
        )
    }
}

impl Drop for ArdMediaStream {
    fn drop(&mut self) {
        self.key_blob.fill(0);
        self.feedback_key_blob.fill(0);
    }
}

impl fmt::Debug for ArdMediaStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ArdMediaStream")
            .field("endpoints", &self.endpoints)
            .field("key_blob_len", &self.key_blob.len())
            .field("feedback_key_blob_len", &self.feedback_key_blob.len())
            .field("codec", &self.codec)
            .field("payload_type", &self.payload_type)
            .field("codec_config", &self.codec_config)
            .field("derived_ssrc", &self.derived_ssrc)
            .field("local_ssrc", &self.local_ssrc)
            .finish()
    }
}

#[derive(Debug)]
struct OutboundMessage {
    payload: Vec<u8>,
    enqueued_at: Instant,
    coalescible_pointer: bool,
    user_input: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArdInputMetrics {
    /// Messages that have not yet been handed to the socket writer.
    pub queue_depth: usize,
    /// Pointer-position states superseded before they reached the network.
    pub coalesced_pointer_moves: u64,
    /// Encrypted records successfully flushed to the socket.
    pub records_written: u64,
    /// Logical RFB messages contained by those records.
    pub messages_written: u64,
    /// Enqueue-to-socket-flush delay of the latest logical message batch.
    pub last_queue_delay: Duration,
    /// Largest observed enqueue-to-socket-flush delay in this connection.
    pub peak_queue_delay: Duration,
    /// Smoothed enqueue-to-socket-flush delay (1/8 update weight).
    pub average_queue_delay: Duration,
    /// Time spent encoding, writing and flushing the latest record.
    pub last_write_duration: Duration,
    /// Largest encoding/write/flush duration in this connection.
    pub peak_write_duration: Duration,
    /// Local monotonic time at which the latest encrypted record finished
    /// flushing to the TCP socket.
    pub last_write_completed_at: Option<Instant>,
    /// Encrypted records containing at least one user input message.
    pub user_input_records_written: u64,
    /// Local monotonic time at which the latest user input record finished
    /// flushing to the TCP socket.
    pub last_user_input_completed_at: Option<Instant>,
}

#[derive(Debug, Default)]
struct InputMetricCounters {
    queue_depth: AtomicUsize,
    coalesced_pointer_moves: AtomicU64,
    records_written: AtomicU64,
    messages_written: AtomicU64,
    last_queue_delay_ns: AtomicU64,
    peak_queue_delay_ns: AtomicU64,
    average_queue_delay_ns: AtomicU64,
    last_write_duration_ns: AtomicU64,
    peak_write_duration_ns: AtomicU64,
    last_write_completed_at: Mutex<Option<Instant>>,
    user_input_records_written: AtomicU64,
    last_user_input_completed_at: Mutex<Option<Instant>>,
}

impl InputMetricCounters {
    fn snapshot(&self) -> ArdInputMetrics {
        ArdInputMetrics {
            queue_depth: self.queue_depth.load(Ordering::Relaxed),
            coalesced_pointer_moves: self.coalesced_pointer_moves.load(Ordering::Relaxed),
            records_written: self.records_written.load(Ordering::Relaxed),
            messages_written: self.messages_written.load(Ordering::Relaxed),
            last_queue_delay: nanos_to_duration(self.last_queue_delay_ns.load(Ordering::Relaxed)),
            peak_queue_delay: nanos_to_duration(self.peak_queue_delay_ns.load(Ordering::Relaxed)),
            average_queue_delay: nanos_to_duration(
                self.average_queue_delay_ns.load(Ordering::Relaxed),
            ),
            last_write_duration: nanos_to_duration(
                self.last_write_duration_ns.load(Ordering::Relaxed),
            ),
            peak_write_duration: nanos_to_duration(
                self.peak_write_duration_ns.load(Ordering::Relaxed),
            ),
            last_write_completed_at: self
                .last_write_completed_at
                .lock()
                .ok()
                .and_then(|completed| *completed),
            user_input_records_written: self.user_input_records_written.load(Ordering::Relaxed),
            last_user_input_completed_at: self
                .last_user_input_completed_at
                .lock()
                .ok()
                .and_then(|completed| *completed),
        }
    }

    fn pointer_coalesced(&self) {
        self.coalesced_pointer_moves.fetch_add(1, Ordering::Relaxed);
    }

    fn record_write(&self, messages: &[OutboundMessage], write_duration: Duration) {
        let now = Instant::now();
        let queue_delay = messages
            .iter()
            .map(|message| now.saturating_duration_since(message.enqueued_at))
            .max()
            .unwrap_or_default();
        let queue_delay_ns = duration_to_nanos(queue_delay);
        let write_duration_ns = duration_to_nanos(write_duration);
        self.last_queue_delay_ns
            .store(queue_delay_ns, Ordering::Relaxed);
        update_atomic_max(&self.peak_queue_delay_ns, queue_delay_ns);
        update_atomic_ema(&self.average_queue_delay_ns, queue_delay_ns);
        self.last_write_duration_ns
            .store(write_duration_ns, Ordering::Relaxed);
        update_atomic_max(&self.peak_write_duration_ns, write_duration_ns);
        if let Ok(mut completed) = self.last_write_completed_at.lock() {
            *completed = Some(now);
        }
        if messages.iter().any(|message| message.user_input) {
            self.user_input_records_written
                .fetch_add(1, Ordering::Relaxed);
            if let Ok(mut completed) = self.last_user_input_completed_at.lock() {
                *completed = Some(now);
            }
        }
        self.records_written.fetch_add(1, Ordering::Relaxed);
        self.messages_written.fetch_add(
            u64::try_from(messages.len()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }
}

fn duration_to_nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn nanos_to_duration(nanos: u64) -> Duration {
    Duration::from_nanos(nanos)
}

fn update_atomic_max(target: &AtomicU64, value: u64) {
    let mut current = target.load(Ordering::Relaxed);
    while value > current {
        match target.compare_exchange_weak(current, value, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(observed) => current = observed,
        }
    }
}

fn update_atomic_ema(target: &AtomicU64, sample: u64) {
    let mut current = target.load(Ordering::Relaxed);
    loop {
        let updated = if current == 0 {
            sample
        } else {
            current.saturating_mul(7).saturating_add(sample) / 8
        };
        match target.compare_exchange_weak(current, updated, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(observed) => current = observed,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum OutboundMode {
    /// Internal RFB/media-stream control, excluded from user-action latency.
    Control,
    Reliable,
    /// A complete pointer state that supersedes a queued position immediately
    /// before it (for example, a button transition at the same coordinates).
    PointerState,
    /// A position-only state. Adjacent unsent states can be replaced by the
    /// newest coordinates without changing any discrete input transition.
    PointerMotion,
}

#[derive(Debug, Default)]
struct OutboundQueueState {
    messages: VecDeque<OutboundMessage>,
    stopped: bool,
}

#[derive(Debug)]
struct OutboundQueue {
    state: Mutex<OutboundQueueState>,
    available: Condvar,
    space: Condvar,
    producers: AtomicUsize,
    metrics: InputMetricCounters,
}

impl OutboundQueue {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(OutboundQueueState::default()),
            available: Condvar::new(),
            space: Condvar::new(),
            producers: AtomicUsize::new(1),
            metrics: InputMetricCounters::default(),
        })
    }

    fn submit(
        &self,
        payload: Vec<u8>,
        mode: OutboundMode,
        blocking: bool,
    ) -> Result<(), ArdClientError> {
        if payload.len() > MAX_OUTBOUND_PAYLOAD_BYTES {
            return Err(ArdClientError::Message(
                "ARD outbound payload exceeds one encrypted record".to_owned(),
            ));
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if state.stopped {
            return Err(ArdClientError::Message(
                "ARD input writer has stopped".to_owned(),
            ));
        }

        if matches!(mode, OutboundMode::PointerMotion)
            && state
                .messages
                .back()
                .is_some_and(|message| message.coalescible_pointer)
        {
            let message = state.messages.back_mut().expect("queue back checked");
            message.payload = payload;
            message.enqueued_at = Instant::now();
            self.metrics.pointer_coalesced();
            return Ok(());
        }

        if matches!(mode, OutboundMode::PointerState)
            && state
                .messages
                .back()
                .is_some_and(|message| message.coalescible_pointer)
        {
            state.messages.pop_back();
            self.metrics.pointer_coalesced();
            self.metrics
                .queue_depth
                .store(state.messages.len(), Ordering::Relaxed);
            self.space.notify_one();
        }

        while state.messages.len() >= MAX_INPUT_QUEUE {
            if !blocking {
                return Err(ArdClientError::Message(
                    "ARD input queue is full".to_owned(),
                ));
            }
            state = self
                .space
                .wait(state)
                .unwrap_or_else(|poison| poison.into_inner());
            if state.stopped {
                return Err(ArdClientError::Message(
                    "ARD input writer has stopped".to_owned(),
                ));
            }
        }

        state.messages.push_back(OutboundMessage {
            payload,
            enqueued_at: Instant::now(),
            coalescible_pointer: matches!(mode, OutboundMode::PointerMotion),
            user_input: !matches!(mode, OutboundMode::Control),
        });
        self.metrics
            .queue_depth
            .store(state.messages.len(), Ordering::Relaxed);
        drop(state);
        self.available.notify_one();
        Ok(())
    }

    fn receive_batch(&self) -> Option<Vec<OutboundMessage>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        while state.messages.is_empty() && !state.stopped {
            if self.producers.load(Ordering::Acquire) == 0 {
                return None;
            }
            state = self
                .available
                .wait(state)
                .unwrap_or_else(|poison| poison.into_inner());
        }
        if state.messages.is_empty() {
            return None;
        }

        let mut payload_bytes = 0usize;
        let mut batch = Vec::new();
        while let Some(next) = state.messages.front() {
            let Some(next_payload_bytes) = payload_bytes.checked_add(next.payload.len()) else {
                break;
            };
            if next_payload_bytes > MAX_OUTBOUND_PAYLOAD_BYTES {
                break;
            }
            payload_bytes = next_payload_bytes;
            batch.push(state.messages.pop_front().expect("queue front checked"));
        }
        self.metrics
            .queue_depth
            .store(state.messages.len(), Ordering::Relaxed);
        drop(state);
        self.space.notify_all();
        Some(batch)
    }

    fn stop(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.stopped = true;
        }
        self.available.notify_all();
        self.space.notify_all();
    }

    fn producer_cloned(&self) {
        self.producers.fetch_add(1, Ordering::Relaxed);
    }

    fn producer_dropped(&self) {
        if self.producers.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.available.notify_all();
        }
    }
}

/// A cloneable, serialized sender for client-side ARD interaction messages.
///
/// All messages share one encrypted-record encoder in a dedicated writer
/// thread. This keeps the CBC chain and record sequence valid even when the
/// GUI thread emits mouse events while the receiver thread is reading frames.
pub struct ArdClientInput {
    queue: Arc<OutboundQueue>,
    writer_error: Arc<Mutex<Option<String>>>,
    supports_extended_scroll: bool,
}

impl Clone for ArdClientInput {
    fn clone(&self) -> Self {
        self.queue.producer_cloned();
        Self {
            queue: Arc::clone(&self.queue),
            writer_error: Arc::clone(&self.writer_error),
            supports_extended_scroll: self.supports_extended_scroll,
        }
    }
}

impl Drop for ArdClientInput {
    fn drop(&mut self) {
        self.queue.producer_dropped();
    }
}

impl fmt::Debug for ArdClientInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ArdClientInput")
            .field("writer_error", &self.writer_error)
            .field("supports_extended_scroll", &self.supports_extended_scroll)
            .field("metrics", &self.metrics())
            .finish()
    }
}

impl ArdClientInput {
    /// Whether the server accepts Apple's extended mouse/scroll input family.
    pub fn supports_extended_scroll(&self) -> bool {
        self.supports_extended_scroll
    }

    /// Returns a lock-free snapshot of the outbound real-time queue.
    pub fn metrics(&self) -> ArdInputMetrics {
        self.queue.metrics.snapshot()
    }

    /// Queues one key press or release using an X11/RFB keysym.
    pub fn send_key_event(&self, pressed: bool, keysym: u32) -> Result<(), ArdClientError> {
        self.submit(
            build_key_event(pressed, keysym).to_vec(),
            OutboundMode::Reliable,
        )
    }

    /// Queues one pointer position/button-mask update.
    pub fn send_pointer_event(
        &self,
        button_mask: u8,
        x: u16,
        y: u16,
    ) -> Result<(), ArdClientError> {
        self.submit(
            build_pointer_event(button_mask, x, y).to_vec(),
            OutboundMode::PointerState,
        )
    }

    /// Queues several pointer updates in order as one outbound payload.
    ///
    /// RFB permits multiple client messages in one encrypted record. Keeping
    /// a scroll gesture in one payload avoids paying the per-record write and
    /// flush cost for every wheel press/release pair.
    pub fn send_pointer_events(&self, events: &[(u8, u16, u16)]) -> Result<(), ArdClientError> {
        if events.is_empty() {
            return Ok(());
        }
        let events_per_record = MAX_OUTBOUND_PAYLOAD_BYTES / 6;
        for chunk in events.chunks(events_per_record) {
            let mut payload = Vec::with_capacity(chunk.len() * 6);
            for &(button_mask, x, y) in chunk {
                payload.extend_from_slice(&build_pointer_event(button_mask, x, y));
            }
            self.submit(payload, OutboundMode::PointerState)?;
        }
        Ok(())
    }

    /// Queues one native Apple scroll-wheel event with precise deltas.
    pub fn send_scroll_wheel_event(
        &self,
        event: ArdScrollWheelEvent,
    ) -> Result<(), ArdClientError> {
        if !self.supports_extended_scroll {
            return Err(ArdClientError::Message(
                "server does not advertise extended scroll input".to_owned(),
            ));
        }
        self.submit(
            build_ard_scroll_wheel_event(event).to_vec(),
            OutboundMode::PointerState,
        )
    }

    /// Queues a pointer update without blocking the GUI event loop when the
    /// network writer is temporarily behind. Button transitions should use
    /// [`Self::send_pointer_event`] so they are never silently dropped.
    pub fn try_send_pointer_event(
        &self,
        button_mask: u8,
        x: u16,
        y: u16,
    ) -> Result<(), ArdClientError> {
        self.try_submit(
            build_pointer_event(button_mask, x, y).to_vec(),
            OutboundMode::PointerMotion,
        )
    }

    /// Queues a coalescible pointer position while allowing a non-GUI
    /// dispatcher thread to wait for bounded queue space. This preserves the
    /// final cursor state without ever building a trail of stale positions.
    pub fn send_pointer_motion(
        &self,
        button_mask: u8,
        x: u16,
        y: u16,
    ) -> Result<(), ArdClientError> {
        self.submit(
            build_pointer_event(button_mask, x, y).to_vec(),
            OutboundMode::PointerMotion,
        )
    }

    /// Queues a UTF-8 clipboard update for the remote desktop.
    pub fn send_clipboard_text(&self, text: &str) -> Result<(), ArdClientError> {
        if text.len() > MAX_CUT_TEXT_BYTES {
            return Err(ArdClientError::Protocol(crate::Error::LimitExceeded(
                "clipboard text",
            )));
        }
        let message = build_client_cut_text(text.as_bytes())?;
        // A standard RFB message may span encrypted records. Split only at
        // record boundaries so large but bounded clipboard contents do not
        // make the CBC writer reject the whole session.
        for chunk in message.chunks(MAX_OUTBOUND_PAYLOAD_BYTES) {
            self.submit(chunk.to_vec(), OutboundMode::Reliable)?;
        }
        Ok(())
    }

    fn submit(&self, payload: Vec<u8>, mode: OutboundMode) -> Result<(), ArdClientError> {
        self.check_writer_error()?;
        self.queue.submit(payload, mode, true)
    }

    fn try_submit(&self, payload: Vec<u8>, mode: OutboundMode) -> Result<(), ArdClientError> {
        self.check_writer_error()?;
        self.queue.submit(payload, mode, false)
    }

    fn check_writer_error(&self) -> Result<(), ArdClientError> {
        let error = self
            .writer_error
            .lock()
            .ok()
            .and_then(|error| error.clone());
        if let Some(error) = error {
            Err(ArdClientError::Message(error))
        } else {
            Ok(())
        }
    }

    fn send_payload(&self, payload: Vec<u8>) -> Result<(), ArdClientError> {
        self.submit(payload, OutboundMode::Control)
    }
}

/// A connected ARD session with framebuffer decoding and bidirectional input.
///
/// MVS output is emitted as tile commands and DCT coefficients so a renderer
/// can expand it on the GPU without materializing a CPU image frame.
pub struct ArdClient {
    stream: TcpStream,
    input: ArdClientInput,
    verified: ArdVerifiedRecordStream,
    dispatcher: ArdMessageDispatcher,
    decoder: Decoder,
    framebuffer: Framebuffer,
    record_scratch: Vec<u8>,
    server_name: String,
    frame_index: u64,
    automatic_updates: bool,
    automatic_frame_interval_ms: u32,
    automatic_updates_started: bool,
    requested_framebuffer_dimensions: Option<(u16, u16)>,
    pending_events: VecDeque<ArdClientEvent>,
    reconnect_config: ArdClientConfig,
    media_host: IpAddr,
    media_offer_sent: bool,
    pending_media_stream: Option<PendingMediaStream>,
}

struct PendingMediaStream {
    endpoints: MediaUdpEndpoints,
    video1_server_to_viewer: Vec<u8>,
    video1_viewer_to_server: Vec<u8>,
    video1_local_ssrc: u32,
}

impl Drop for PendingMediaStream {
    fn drop(&mut self) {
        self.video1_server_to_viewer.fill(0);
        self.video1_viewer_to_server.fill(0);
    }
}

impl ArdClient {
    pub fn connect(mut config: ArdClientConfig) -> Result<Self, ArdClientError> {
        let result = Self::connect_inner(&mut config);
        config.password.fill(0);
        result
    }

    /// Re-establishes the authenticated session using the original
    /// connection configuration. The framebuffer, decoder, encryption
    /// sequence and input writer are all replaced with a fresh session.
    pub fn reconnect(&mut self) -> Result<(), ArdClientError> {
        let mut config = self.reconnect_config.clone();
        let result = Self::connect_inner(&mut config);
        config.password.fill(0);
        let replacement = result?;
        *self = replacement;
        Ok(())
    }

    fn connect_inner(config: &mut ArdClientConfig) -> Result<Self, ArdClientError> {
        config.media_udp_port_overrides.validate()?;
        let mut stream = TcpStream::connect(&config.address)?;
        let media_host = stream.peer_addr()?.ip();
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(config.timeout))?;
        stream.set_write_timeout(Some(config.timeout))?;

        let banner = read_exact_vector(&mut stream, 12)?;
        if ProtocolVersion::parse(&banner)? != ProtocolVersion::ARD_3_889 {
            return Err(ArdClientError::Message(
                "server did not offer ARD protocol 3.889".to_owned(),
            ));
        }
        stream.write_all(&ProtocolVersion::ARD_3_889.banner()?)?;

        let security_count = usize::from(read_exact_vector(&mut stream, 1)?[0]);
        let mut security_offer = vec![security_count as u8];
        security_offer.extend_from_slice(&read_exact_vector(&mut stream, security_count)?);
        let (security_types, consumed) = parse_security_types(&security_offer, 36)?;
        if consumed != security_offer.len()
            || !security_types
                .iter()
                .any(|kind| matches!(kind, SecurityType::Apple(30)))
        {
            return Err(ArdClientError::Message(
                "server did not offer Apple security type 30".to_owned(),
            ));
        }
        if security_types.len() != 1 {
            stream.write_all(&[30])?;
        }

        let mut challenge_wire = read_exact_vector(&mut stream, 4)?;
        let key_len = usize::from(u16::from_be_bytes([challenge_wire[2], challenge_wire[3]]));
        let challenge_tail_len = key_len.checked_mul(2).ok_or_else(|| {
            ArdClientError::Message("authentication key length overflow".to_owned())
        })?;
        challenge_wire.extend_from_slice(&read_exact_vector(&mut stream, challenge_tail_len)?);
        let (challenge, consumed) = parse_ard_auth_challenge(&challenge_wire, MAX_KEY_BYTES)?;
        if consumed != challenge_wire.len() {
            return Err(ArdClientError::Message(
                "trailing authentication challenge bytes".to_owned(),
            ));
        }

        let mut private_random = vec![0_u8; key_len.saturating_mul(2)];
        getrandom::fill(&mut private_random)
            .map_err(|error| ArdClientError::Message(format!("random source failed: {error}")))?;
        let mut credential_noise = [0_u8; 128];
        getrandom::fill(&mut credential_noise)
            .map_err(|error| ArdClientError::Message(format!("random source failed: {error}")))?;
        let exchange_result = build_ard_type30_client_exchange(
            &challenge,
            &config.username,
            &config.password,
            &private_random,
            credential_noise,
            MAX_KEY_BYTES,
        );
        private_random.fill(0);
        credential_noise.fill(0);
        let exchange = exchange_result?;
        stream.write_all(&exchange.response().encrypted_credentials)?;
        stream.write_all(&exchange.response().client_public_key)?;
        stream.flush()?;

        let (_, mut authentication_value) = exchange.into_parts();
        let security_result = u32::from_be_bytes(
            read_exact_vector(&mut stream, 4)?
                .try_into()
                .expect("security result has fixed length"),
        );
        if security_result != 0 {
            authentication_value.fill(0);
            return Err(ArdClientError::Message(format!(
                "Screen Sharing authentication failed with status {security_result}"
            )));
        }

        stream.write_all(&[0xc1])?;
        let mut init_wire = read_exact_vector(&mut stream, 24)?;
        let payload_len = usize::try_from(u32::from_be_bytes(
            init_wire[20..24]
                .try_into()
                .expect("ServerInit length has fixed width"),
        ))
        .map_err(|_| ArdClientError::Message("ServerInit length overflow".to_owned()))?;
        if payload_len > MAX_SERVER_NAME_BYTES {
            authentication_value.fill(0);
            return Err(ArdClientError::Message(
                "ServerInit is too large".to_owned(),
            ));
        }
        init_wire.extend_from_slice(&read_exact_vector(&mut stream, payload_len)?);
        let (server_init, consumed) = parse_server_init(&init_wire, MAX_SERVER_NAME_BYTES)?;
        if consumed != init_wire.len() {
            authentication_value.fill(0);
            return Err(ArdClientError::Message(
                "trailing ServerInit bytes".to_owned(),
            ));
        }
        if !server_init
            .extension
            .as_ref()
            .is_some_and(|extension| extension.supports_command(0x12))
        {
            authentication_value.fill(0);
            return Err(ArdClientError::Message(
                "server does not advertise encrypted transport".to_owned(),
            ));
        }

        let supports_extended_scroll = server_init
            .extension
            .as_ref()
            .is_some_and(|extension| extension.supports_command(0x17));
        let supports_display_configuration = server_init
            .extension
            .as_ref()
            .is_some_and(|extension| extension.supports_command(0x1d));
        if config.display_configuration.is_some() && !supports_display_configuration {
            authentication_value.fill(0);
            return Err(ArdClientError::Message(
                "server does not advertise display configuration support".to_owned(),
            ));
        }
        let requested_pixel_format = config.output_format.pixel_format(server_init.pixel_format);
        let (mut decoder, mut framebuffer) = if config.video_quality == ArdVideoQuality::Adaptive
            || config.video_quality.is_high_performance()
        {
            (
                Decoder::new_gpu_mvs(requested_pixel_format)?,
                Framebuffer::new_metadata_with_format(
                    server_init.width,
                    server_init.height,
                    config
                        .output_format
                        .framebuffer_format(requested_pixel_format),
                )?,
            )
        } else {
            (
                Decoder::new(requested_pixel_format)?,
                Framebuffer::new_with_format(
                    server_init.width,
                    server_init.height,
                    config
                        .output_format
                        .framebuffer_format(requested_pixel_format),
                )?,
            )
        };
        stream.write_all(&[10, 0, 0, 1])?;
        stream.write_all(&viewer_information())?;
        stream.write_all(&build_set_pixel_format(requested_pixel_format)?)?;
        stream.write_all(&build_set_encodings(config.video_quality.encodings())?)?;
        stream.write_all(&build_ard_set_encryption_level(1, &[1])?)?;
        // This request lets servers serialize the 1103 control as the update
        // satisfying the initial non-incremental request. Current macOS also
        // accepts the proposal before this message.
        stream.write_all(&build_framebuffer_update_request(
            false,
            0,
            0,
            server_init.width,
            server_init.height,
        ))?;
        stream.flush()?;

        let control = read_encryption_control(&mut stream, &mut decoder, &mut framebuffer)?;
        let material = unwrap_ard_session_material(&control, authentication_value);
        authentication_value.fill(0);
        stream.write_all(&build_ard_encryption_activation())?;

        let mut encoder = material.record_encoder(MAX_RECORD_BYTES)?;
        let verified = ArdVerifiedRecordStream::new(
            material.record_decoder(MAX_RECORD_BYTES)?,
            MAX_RECORD_BYTES,
            16,
        )?;
        let automatic_frame_interval_ms = if config.automatic_updates {
            u32::try_from(config.frame_interval.as_millis()).map_err(|_| {
                ArdClientError::Message("automatic frame interval is too large".to_owned())
            })?
        } else {
            0
        };
        if let Some(configuration) = &config.display_configuration {
            let request = build_ard_set_display_configuration(configuration)?;
            stream.write_all(&encoder.encode_wire(&request)?)?;
            stream.flush()?;
        }
        // Apple's view startup always requests one non-incremental frame.
        // That frame establishes MVS copy/cache state before type 9 enables
        // the server-driven incremental stream.
        let requested_framebuffer_dimensions = config
            .display_configuration
            .as_ref()
            .map(ArdDisplayConfiguration::single_backing_dimensions)
            .transpose()?
            .flatten();
        let requested_framebuffer =
            requested_framebuffer_dimensions.unwrap_or((server_init.width, server_init.height));
        let request = build_framebuffer_update_request(
            false,
            0,
            0,
            requested_framebuffer.0,
            requested_framebuffer.1,
        );
        stream.write_all(&encoder.encode_wire(&request)?)?;
        stream.flush()?;
        // Incremental RFB requests are allowed to remain pending while the
        // desktop is unchanged. Keep handshake operations bounded, then let
        // the receive-only stream wait without treating an idle screen as a
        // disconnect.
        stream.set_read_timeout(None)?;

        let writer_stream = stream.try_clone()?;
        let queue = OutboundQueue::new();
        let writer_error = Arc::new(Mutex::new(None));
        let input = ArdClientInput {
            queue: Arc::clone(&queue),
            writer_error: writer_error.clone(),
            supports_extended_scroll,
        };
        spawn_input_writer(writer_stream, encoder, queue, writer_error);

        Ok(Self {
            stream,
            input,
            verified,
            dispatcher: ArdMessageDispatcher::new(MAX_MESSAGE_BYTES, MAX_CUT_TEXT_BYTES)?,
            decoder,
            framebuffer,
            record_scratch: Vec::new(),
            server_name: server_init.name,
            frame_index: 0,
            automatic_updates: config.automatic_updates,
            automatic_frame_interval_ms,
            automatic_updates_started: false,
            requested_framebuffer_dimensions,
            pending_events: VecDeque::new(),
            reconnect_config: config.clone(),
            media_host,
            media_offer_sent: false,
            pending_media_stream: None,
        })
    }

    pub fn framebuffer(&self) -> &Framebuffer {
        &self.framebuffer
    }

    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    fn frame_request_dimensions(&self) -> (u16, u16) {
        self.requested_framebuffer_dimensions
            .unwrap_or((self.framebuffer.width(), self.framebuffer.height()))
    }

    /// Bounds how long [`Self::next_event`] may wait for another encrypted
    /// server record. The viewer uses this only while strict AVC media
    /// negotiation is pending, then restores an unbounded idle wait.
    pub fn set_event_read_timeout(&self, timeout: Option<Duration>) -> Result<(), ArdClientError> {
        self.stream.set_read_timeout(timeout)?;
        Ok(())
    }

    /// Returns a cloneable handle for GUI or application input dispatch.
    pub fn input(&self) -> ArdClientInput {
        self.input.clone()
    }

    pub fn send_key_event(&self, pressed: bool, keysym: u32) -> Result<(), ArdClientError> {
        self.input.send_key_event(pressed, keysym)
    }

    pub fn send_pointer_event(
        &self,
        button_mask: u8,
        x: u16,
        y: u16,
    ) -> Result<(), ArdClientError> {
        self.input.send_pointer_event(button_mask, x, y)
    }

    pub fn send_clipboard_text(&self, text: &str) -> Result<(), ArdClientError> {
        self.input.send_clipboard_text(text)
    }

    pub fn take_gpu_mvs_frames(&mut self) -> Vec<crate::MvsGpuFrame> {
        self.decoder.take_gpu_mvs_frames()
    }

    pub fn drain_gpu_mvs_frames(&mut self, visit: impl FnMut(crate::MvsGpuFrame)) {
        self.decoder.drain_gpu_mvs_frames(visit);
    }

    fn handle_media_stream_reply(
        &mut self,
        reply: MediaStreamServerReply,
    ) -> Result<Option<ArdClientEvent>, ArdClientError> {
        match reply {
            MediaStreamServerReply::Message1(message) => {
                if message.video1_port == 0 {
                    return Err(ArdClientError::Message(
                        "AVC media stream did not provide a video1 UDP port".to_owned(),
                    ));
                }
                let Some(preferred_codec) =
                    self.reconnect_config.video_quality.preferred_media_codec()
                else {
                    return Ok(None);
                };
                if self.media_offer_sent {
                    return Ok(None);
                }

                let mut session_id = [0_u8; 16];
                getrandom::fill(&mut session_id).map_err(|error| {
                    ArdClientError::Message(format!("AVC session random source failed: {error}"))
                })?;
                let mut audio_viewer_to_server = [0_u8; crate::media_stream::MEDIA_STREAM_KEY_LEN];
                let mut audio_server_to_viewer = [0_u8; crate::media_stream::MEDIA_STREAM_KEY_LEN];
                let mut video1_viewer_to_server = [0_u8; crate::media_stream::MEDIA_STREAM_KEY_LEN];
                let mut video1_server_to_viewer = [0_u8; crate::media_stream::MEDIA_STREAM_KEY_LEN];
                for key in [
                    &mut audio_viewer_to_server,
                    &mut audio_server_to_viewer,
                    &mut video1_viewer_to_server,
                    &mut video1_server_to_viewer,
                ] {
                    getrandom::fill(key).map_err(|error| {
                        ArdClientError::Message(format!("AVC key random source failed: {error}"))
                    })?;
                }
                let keys = MediaStreamKeyMaterial::new(
                    &audio_viewer_to_server,
                    &audio_server_to_viewer,
                    &video1_viewer_to_server,
                    &video1_server_to_viewer,
                    None,
                    None,
                )?;
                let call_id = format_uuid(session_id);
                let mut video_call_id_bytes = [0_u8; 16];
                getrandom::fill(&mut video_call_id_bytes).map_err(|error| {
                    ArdClientError::Message(format!("AVC call ID random source failed: {error}"))
                })?;
                let video_call_id = format_uuid(video_call_id_bytes);
                let audio_ssrc = generate_media_ssrc()?;
                let video1_derived_ssrc = generate_media_ssrc()?;
                // ScreenSharing's negotiator expects a VCCallInfoBlob with a
                // recognizable Apple build string. Keep the protocol profile
                // stable even when the Rust client is running on another OS.
                let endpoint_info = build_remote_endpoint_info("Mac16,12", "25G72");
                let configuration = MediaStreamConfiguration {
                    message_version: MEDIA_STREAM_MESSAGE_VERSION,
                    flags: MediaStreamFlags::new(
                        MediaStreamFlags::VIDEO1_60FPS
                            | MediaStreamFlags::SEND_CURSOR
                            | MediaStreamFlags::VIEWER_APP,
                    ),
                    session_id,
                    audio_offer: build_media_stream_offer_with_ssrc(
                        &call_id,
                        &endpoint_info,
                        8,
                        1,
                        audio_ssrc,
                    )?,
                    video1_offer: build_media_stream_offer_with_ssrc_and_codec(
                        &video_call_id,
                        &endpoint_info,
                        7,
                        2,
                        video1_derived_ssrc,
                        preferred_codec,
                    )?,
                    video2_offer: None,
                    keys,
                };
                let video1_key_blob = video1_server_to_viewer.to_vec();
                let video1_feedback_key_blob = video1_viewer_to_server.to_vec();
                let offer = configuration.encode()?;
                debug_assert_eq!(offer[0], CLIENT_MEDIA_STREAM_MESSAGE_TYPE);
                self.input.send_payload(offer)?;
                drop(configuration);
                session_id.fill(0);
                audio_viewer_to_server.fill(0);
                audio_server_to_viewer.fill(0);
                video1_viewer_to_server.fill(0);
                video1_server_to_viewer.fill(0);
                video_call_id_bytes.fill(0);
                self.media_offer_sent = true;
                let endpoints = MediaUdpEndpoints::from_message1(self.media_host, &message)
                    .with_remote_port_overrides(self.reconnect_config.media_udp_port_overrides)?;
                self.pending_media_stream = Some(PendingMediaStream {
                    endpoints,
                    video1_server_to_viewer: video1_key_blob,
                    video1_viewer_to_server: video1_feedback_key_blob,
                    video1_local_ssrc: video1_derived_ssrc,
                });
                if self.automatic_updates && !self.automatic_updates_started {
                    let (width, height) = self.frame_request_dimensions();
                    let request = build_ard_auto_frame_update(
                        self.automatic_frame_interval_ms,
                        0,
                        0,
                        width,
                        height,
                    );
                    self.input.send_payload(request.to_vec())?;
                    self.automatic_updates_started = true;
                } else if !self.automatic_updates {
                    let (width, height) = self.frame_request_dimensions();
                    let request = build_framebuffer_update_request(true, 0, 0, width, height);
                    self.input.send_payload(request.to_vec())?;
                }
                // Message1 only supplies the endpoint and key. Wait for the
                // negotiator answer before starting SRTP: its media blob
                // carries the server stream SSRC.
                Ok(None)
            }
            MediaStreamServerReply::Answer(MediaStreamAnswer { answer_body, .. }) => {
                let parsed = MediaStreamOffer::parse(&answer_body).map_err(|error| {
                    ArdClientError::Message(format!("AVC negotiator answer parse failed: {error}"))
                })?;
                let Some(mut pending) = self.pending_media_stream.take() else {
                    return Ok(None);
                };
                let derived_ssrc = parsed.remote_ssrc.ok_or_else(|| {
                    ArdClientError::Message(
                        "AVC negotiator answer did not provide the remote video SSRC".to_owned(),
                    )
                })?;
                let mut codec_config = parsed.codec;
                // Native Message2 can omit separate codec/payload selection
                // fields. Resolve the payload mapping for the single codec
                // requested by this quality profile.
                let preferred_codec = self
                    .reconnect_config
                    .video_quality
                    .preferred_media_codec()
                    .ok_or_else(|| {
                        ArdClientError::Message(
                            "received AVC answer outside high-performance mode".to_owned(),
                        )
                    })?;
                if let Some(mapping) = codec_config
                    .payload_mappings
                    .iter()
                    .find(|mapping| mapping.codec == Some(preferred_codec))
                {
                    codec_config.payload_type = Some(mapping.payload_type);
                    codec_config.codec = mapping.codec;
                    if !mapping.encoding_name.is_empty() {
                        codec_config.encoding_name = Some(mapping.encoding_name.clone());
                    }
                } else if codec_config.codec != Some(preferred_codec) {
                    return Err(ArdClientError::Message(format!(
                        "AVC negotiator did not accept requested codec {}",
                        preferred_codec.name()
                    )));
                }
                let codec = codec_config.codec.ok_or_else(|| {
                    ArdClientError::Message(format!(
                        "AVC negotiator selected unsupported codec: {codec_config:?}"
                    ))
                })?;
                let payload_type = codec_config.payload_type.ok_or_else(|| {
                    ArdClientError::Message(
                        "AVC negotiator answer did not provide a selected RTP payload type"
                            .to_owned(),
                    )
                })?;
                let key_blob = core::mem::take(&mut pending.video1_server_to_viewer);
                let feedback_key_blob = core::mem::take(&mut pending.video1_viewer_to_server);
                Ok(Some(ArdClientEvent::MediaStream(Box::new(
                    ArdMediaStream {
                        endpoints: pending.endpoints,
                        key_blob,
                        feedback_key_blob,
                        codec,
                        payload_type,
                        codec_config,
                        derived_ssrc,
                        local_ssrc: pending.video1_local_ssrc,
                    },
                ))))
            }
            MediaStreamServerReply::Error(error) => Err(ArdClientError::Message(format!(
                "server rejected AVC media stream (type={}, subcode={})",
                error.error_type, error.error_sub_code
            ))),
        }
    }

    /// Reads the next decoded session event.
    pub fn next_event(&mut self) -> Result<ArdClientEvent, ArdClientError> {
        let policy = self.reconnect_config.reconnect;
        let mut attempts = 0_usize;
        loop {
            match self.next_event_once() {
                Ok(event) => return Ok(event),
                Err(error) if error.is_io() && attempts < policy.max_attempts => {
                    attempts += 1;
                    if !policy.delay.is_zero() {
                        thread::sleep(policy.delay);
                    }
                    match self.reconnect() {
                        Ok(()) => return Ok(ArdClientEvent::Reconnected),
                        Err(error) if error.is_io() && attempts < policy.max_attempts => {}
                        Err(error) => return Err(error),
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn next_event_once(&mut self) -> Result<ArdClientEvent, ArdClientError> {
        if let Some(event) = self.pending_events.pop_front() {
            return Ok(event);
        }
        let mut wire_bytes = 0_usize;
        loop {
            self.input.check_writer_error()?;
            let record_sequence = self.verified.sequence();
            let wire_bytes_for_record =
                read_encrypted_record(&mut self.stream, &mut self.record_scratch)?;
            wire_bytes = wire_bytes.saturating_add(wire_bytes_for_record);
            let mut framebuffer_updates = 0_usize;
            let mut rectangle_count = 0_usize;
            let mut payload_bytes = 0_usize;
            self.verified
                .decode_record_in_place(&mut self.record_scratch)
                .map_err(|error| {
                    ArdClientError::Message(format!(
                        "校验或解密服务端记录 #{record_sequence} 失败：{error}"
                    ))
                })?;
            let payload = &self.record_scratch;
            let messages = self
                .dispatcher
                .push(payload, &mut self.decoder, &mut self.framebuffer)
                .map_err(|error| {
                    let payload_prefix = payload
                        .iter()
                        .take(32)
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<Vec<_>>()
                        .join(" ");
                    ArdClientError::Message(format!(
                        "解析或解码服务端记录 #{record_sequence} 失败（已缓冲 {} 字节，负载前缀 {payload_prefix}）：{error}",
                        self.dispatcher.buffered_bytes(),
                    ))
                })?;
            let mut batch_events = Vec::new();
            let mut frame_event_position = None;
            for message in messages {
                match message {
                    ArdServerMessage::FramebufferUpdate {
                        rectangle_count: rectangles,
                        bytes,
                    } => {
                        if frame_event_position.is_none() {
                            frame_event_position = Some(batch_events.len());
                        }
                        framebuffer_updates = framebuffer_updates.saturating_add(1);
                        rectangle_count = rectangle_count.saturating_add(rectangles);
                        payload_bytes = payload_bytes.saturating_add(bytes);
                    }
                    ArdServerMessage::ServerCutText(text) => {
                        batch_events.push(ArdClientEvent::Clipboard(text));
                    }
                    ArdServerMessage::Bell => batch_events.push(ArdClientEvent::Bell),
                    ArdServerMessage::StateChange => batch_events.push(ArdClientEvent::StateChange),
                    ArdServerMessage::EncryptionControl(_) => {}
                    ArdServerMessage::MediaStream(reply) => {
                        if let Some(event) = self.handle_media_stream_reply(reply)? {
                            batch_events.push(event);
                        }
                    }
                }
            }
            if let Some(frame_event_position) = frame_event_position {
                self.frame_index = self.frame_index.wrapping_add(framebuffer_updates as u64);
                if self.automatic_updates && !self.automatic_updates_started {
                    let (width, height) = self.frame_request_dimensions();
                    let request = build_ard_auto_frame_update(
                        self.automatic_frame_interval_ms,
                        0,
                        0,
                        width,
                        height,
                    );
                    self.input.send_payload(request.to_vec())?;
                    self.automatic_updates_started = true;
                } else if !self.automatic_updates {
                    let (width, height) = self.frame_request_dimensions();
                    let request = build_framebuffer_update_request(true, 0, 0, width, height);
                    self.input.send_payload(request.to_vec())?;
                }
                batch_events.insert(
                    frame_event_position,
                    ArdClientEvent::Frame(ArdFrameInfo {
                        index: self.frame_index,
                        framebuffer_updates,
                        rectangle_count,
                        payload_bytes,
                        wire_bytes,
                    }),
                );
            }
            self.pending_events.extend(batch_events);
            if let Some(event) = self.pending_events.pop_front() {
                return Ok(event);
            }
        }
    }

    /// Compatibility helper that skips non-frame events. New callers should
    /// use [`Self::next_event`] to receive clipboard and bell notifications.
    pub fn next_frame(&mut self) -> Result<ArdFrameInfo, ArdClientError> {
        loop {
            if let ArdClientEvent::Frame(frame) = self.next_event()? {
                return Ok(frame);
            }
        }
    }
}

fn spawn_input_writer(
    mut stream: TcpStream,
    mut encoder: crate::ArdSessionRecordEncoder,
    queue: Arc<OutboundQueue>,
    writer_error: Arc<Mutex<Option<String>>>,
) {
    thread::spawn(move || {
        while let Some(messages) = queue.receive_batch() {
            let payload_bytes = messages.iter().map(|message| message.payload.len()).sum();
            let mut payload = Vec::with_capacity(payload_bytes);
            for message in &messages {
                payload.extend_from_slice(&message.payload);
            }
            let write_started = Instant::now();
            let result = encoder
                .encode_wire(&payload)
                .map_err(ArdClientError::from)
                .and_then(|wire| {
                    stream.write_all(&wire)?;
                    stream.flush()?;
                    Ok(())
                });
            match result {
                Ok(()) => queue
                    .metrics
                    .record_write(&messages, write_started.elapsed()),
                Err(error) => {
                    if let Ok(mut current) = writer_error.lock() {
                        *current = Some(format!("ARD input writer failed: {error}"));
                    }
                    queue.stop();
                    break;
                }
            }
        }
        queue.stop();
    });
}

fn format_uuid(bytes: [u8; 16]) -> String {
    format!(
        "{:08X}-{:04X}-{:04X}-{:04X}-{:012X}",
        u32::from_be_bytes(bytes[0..4].try_into().expect("UUID width")),
        u16::from_be_bytes(bytes[4..6].try_into().expect("UUID width")),
        u16::from_be_bytes(bytes[6..8].try_into().expect("UUID width")),
        u16::from_be_bytes(bytes[8..10].try_into().expect("UUID width")),
        u64::from_be_bytes([
            0, 0, bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        ])
    )
}

fn read_exact_vector(stream: &mut TcpStream, len: usize) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0; len];
    stream.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn read_encrypted_record(
    stream: &mut TcpStream,
    ciphertext: &mut Vec<u8>,
) -> Result<usize, ArdClientError> {
    let mut length = [0_u8; 2];
    stream.read_exact(&mut length)?;
    let ciphertext_len = usize::from(u16::from_be_bytes(length));
    if ciphertext_len == 0 || !ciphertext_len.is_multiple_of(16) {
        return Err(ArdClientError::Message(
            "invalid encrypted-record length".to_owned(),
        ));
    }
    ciphertext.resize(ciphertext_len, 0);
    stream.read_exact(ciphertext)?;
    Ok(2 + ciphertext_len)
}

fn read_encryption_control(
    stream: &mut TcpStream,
    decoder: &mut Decoder,
    framebuffer: &mut Framebuffer,
) -> Result<ArdEncryptionControl, ArdClientError> {
    loop {
        let message_type = read_exact_vector(stream, 1)?[0];
        match message_type {
            0 => {
                let mut update = vec![message_type];
                update.extend_from_slice(&read_exact_vector(stream, 3)?);
                let count = u16::from_be_bytes([update[2], update[3]]);
                for _ in 0..count {
                    let rectangle = read_exact_vector(stream, 12)?;
                    let encoding = i32::from_be_bytes(
                        rectangle[8..12]
                            .try_into()
                            .expect("rectangle encoding has fixed width"),
                    );
                    update.extend_from_slice(&rectangle);
                    if encoding != 1103 {
                        return Err(ArdClientError::Message(format!(
                            "expected encryption control, received encoding {encoding}"
                        )));
                    }
                    update.extend_from_slice(&read_exact_vector(
                        stream,
                        ArdEncryptionControl::WIRE_LEN,
                    )?);
                }
                let consumed = parse_framebuffer_update(&update, decoder, framebuffer)?;
                if consumed != update.len() {
                    return Err(ArdClientError::Message(
                        "trailing encryption-control bytes".to_owned(),
                    ));
                }
                if let Some(control) = decoder.take_ard_encryption_control() {
                    return Ok(control);
                }
            }
            2 => {}
            3 => {
                let header = read_exact_vector(stream, 7)?;
                let text_len = usize::try_from(u32::from_be_bytes(
                    header[3..7]
                        .try_into()
                        .expect("cut text length has fixed width"),
                ))
                .map_err(|_| ArdClientError::Message("cut text length overflow".to_owned()))?;
                if text_len > MAX_CUT_TEXT_BYTES {
                    return Err(ArdClientError::Message("cut text is too large".to_owned()));
                }
                let _ = read_exact_vector(stream, text_len)?;
            }
            0x14 => {
                // Native screensharingd emits an eight-byte state-change
                // notification while the session is being established.
                let _ = read_exact_vector(stream, 7)?;
            }
            other => {
                return Err(ArdClientError::Message(format!(
                    "unexpected plaintext server message {other}"
                )));
            }
        }
    }
}

fn viewer_information() -> [u8; ArdViewerInformation::WIRE_LEN] {
    let mut message = [0; ArdViewerInformation::WIRE_LEN];
    message[0] = ArdViewerInformation::MESSAGE_TYPE;
    message[2..4].copy_from_slice(&(ArdViewerInformation::PAYLOAD_LEN as u16).to_be_bytes());
    message[4..6].copy_from_slice(&ArdViewerInformation::VERSION.to_be_bytes());
    for (index, component) in [2_u32, 6, 1, 0].into_iter().enumerate() {
        let offset = 6 + index * 4;
        message[offset..offset + 4].copy_from_slice(&component.to_be_bytes());
    }
    // Preserve the complete capability profile observed from the native
    // Screen Sharing client. Sending only the four leading components left
    // the server with an all-zero feature block and made its stream selection
    // differ from the native path this viewer is intended to match.
    for (index, component) in [26_u32, 5, 2].into_iter().enumerate() {
        let offset = 22 + index * 4;
        message[offset..offset + 4].copy_from_slice(&component.to_be_bytes());
    }
    message[34] = 0xb0;
    message[36] = 0x0c;
    message[37] = 0x03;
    message[38] = 0x90;
    message[44] = 0x40;
    message
}

#[cfg(test)]
mod outbound_queue_tests {
    use super::*;

    #[test]
    fn high_performance_profiles_do_not_advertise_a_visual_fallback() {
        assert_eq!(
            ArdVideoQuality::HighPerformanceHevc.encodings(),
            [ENCODING_AVC_MEDIA_STREAM, -223]
        );
        assert_eq!(
            ArdVideoQuality::HighPerformanceAvc.encodings(),
            [ENCODING_AVC_MEDIA_STREAM, -223]
        );
    }

    #[test]
    fn adjacent_pointer_motion_keeps_only_the_latest_unsent_state() {
        let queue = OutboundQueue::new();
        for coordinate in 0_u16..1_000 {
            queue
                .submit(
                    coordinate.to_be_bytes().to_vec(),
                    OutboundMode::PointerMotion,
                    false,
                )
                .expect("position state should coalesce");
        }

        let metrics = queue.metrics.snapshot();
        assert_eq!(metrics.queue_depth, 1);
        assert_eq!(metrics.coalesced_pointer_moves, 999);
        let batch = queue.receive_batch().expect("latest pointer state");
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].payload, 999_u16.to_be_bytes());
    }

    #[test]
    fn pointer_transition_replaces_only_the_immediately_preceding_motion() {
        let queue = OutboundQueue::new();
        queue
            .submit(vec![0x10], OutboundMode::Reliable, true)
            .expect("reliable command");
        queue
            .submit(vec![0x20], OutboundMode::PointerMotion, false)
            .expect("pointer motion");
        queue
            .submit(vec![0x21], OutboundMode::PointerMotion, false)
            .expect("newer pointer motion");
        queue
            .submit(vec![0x30], OutboundMode::PointerState, true)
            .expect("button transition");

        let batch = queue.receive_batch().expect("outbound batch");
        let payloads: Vec<_> = batch.into_iter().map(|message| message.payload).collect();
        assert_eq!(payloads, [vec![0x10], vec![0x30]]);
        assert_eq!(queue.metrics.snapshot().coalesced_pointer_moves, 2);
    }

    #[test]
    fn pointer_coalescing_never_crosses_a_reliable_ordering_barrier() {
        let queue = OutboundQueue::new();
        queue
            .submit(vec![0x10], OutboundMode::PointerMotion, false)
            .expect("first pointer state");
        queue
            .submit(vec![0x20], OutboundMode::Reliable, true)
            .expect("ordering barrier");
        queue
            .submit(vec![0x30], OutboundMode::PointerMotion, false)
            .expect("second pointer state");
        queue
            .submit(vec![0x31], OutboundMode::PointerMotion, false)
            .expect("newest pointer state");

        let batch = queue.receive_batch().expect("outbound batch");
        let payloads: Vec<_> = batch.into_iter().map(|message| message.payload).collect();
        assert_eq!(payloads, [vec![0x10], vec![0x20], vec![0x31]]);
    }

    #[test]
    fn batching_respects_the_encrypted_record_payload_limit() {
        let queue = OutboundQueue::new();
        queue
            .submit(
                vec![0xaa; MAX_OUTBOUND_PAYLOAD_BYTES - 1],
                OutboundMode::Reliable,
                true,
            )
            .expect("first payload");
        queue
            .submit(vec![0xbb; 2], OutboundMode::Reliable, true)
            .expect("second payload");

        assert_eq!(queue.receive_batch().expect("first record").len(), 1);
        assert_eq!(queue.receive_batch().expect("second record").len(), 1);
    }

    #[test]
    fn internal_control_records_do_not_count_as_user_actions() {
        let queue = OutboundQueue::new();
        queue
            .submit(vec![0x10], OutboundMode::Control, true)
            .expect("control payload");
        let control = queue.receive_batch().expect("control record");
        queue.metrics.record_write(&control, Duration::ZERO);
        assert_eq!(queue.metrics.snapshot().user_input_records_written, 0);

        queue
            .submit(vec![0x20], OutboundMode::Reliable, true)
            .expect("user input payload");
        let input = queue.receive_batch().expect("input record");
        queue.metrics.record_write(&input, Duration::ZERO);
        let metrics = queue.metrics.snapshot();
        assert_eq!(metrics.user_input_records_written, 1);
        assert!(metrics.last_user_input_completed_at.is_some());
    }
}
