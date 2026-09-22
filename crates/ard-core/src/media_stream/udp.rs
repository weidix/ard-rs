//! UDP transport for Apple's real-time media stream.
//!
//! The server hands out media ports in `MediaStreamMessage1`; the client
//! binds the same local port per stream and connects to the explicit remote
//! endpoint for each stream (video1 is base+1 on the confirmed build).
//! Packets are standard RTP (RFC 3550) with Apple's AVC SRTP payload
//! encryption and suite-5 authentication suffixes.

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::raw_stream::{RawStreamKind, RawStreamSink};
use crate::{Error, Result};

use super::MAX_RTP_PACKET;
use super::assembler::VideoStreamAssembler;

/// The four horizontal bands one desktop frame is coded as. It lives with the
/// assembler and is re-exported here because callers of this module have always
/// named it here.
pub use super::assembler::AVC_VIDEO_SLICE_COUNT;
use super::negotiation::MediaStreamCodec;
use super::rtp::{AccessUnit, RtpPacket};
use super::srtp::{SrtcpContext, SrtpContext};
use super::wire::MediaStreamMessage1;

const RTCP_REPORT_INTERVAL: Duration = Duration::from_secs(1);
const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_millis(100);
const UDP_READ_TIMEOUT: Duration = Duration::from_millis(10);
/// Which media stream a UDP socket carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UdpStreamKind {
    Video1,
    Video2,
    Audio,
}

impl UdpStreamKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Video1 => "video1",
            Self::Video2 => "video2",
            Self::Audio => "audio",
        }
    }
}

/// Optional remote UDP destination ports for a forwarded AVC media session.
///
/// Screen Sharing advertises the ports that the viewer must bind locally in
/// `MediaStreamMessage1`. A router may expose the remote Mac on different
/// external ports, so these values replace only the remote destination. They
/// deliberately do not change the negotiated local bind ports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MediaUdpPortOverrides {
    pub video1: Option<u16>,
    pub video2: Option<u16>,
    pub audio: Option<u16>,
}

impl MediaUdpPortOverrides {
    pub const fn is_empty(self) -> bool {
        self.video1.is_none() && self.video2.is_none() && self.audio.is_none()
    }

    pub fn validate(self) -> Result<()> {
        if [self.video1, self.video2, self.audio]
            .into_iter()
            .flatten()
            .any(|port| port == 0)
        {
            return Err(Error::Invalid(
                "remote media UDP port override must be non-zero",
            ));
        }
        Ok(())
    }

    pub const fn port_for(self, kind: UdpStreamKind) -> Option<u16> {
        match kind {
            UdpStreamKind::Video1 => self.video1,
            UdpStreamKind::Video2 => self.video2,
            UdpStreamKind::Audio => self.audio,
        }
    }
}

/// Remote UDP endpoints derived from the server's base port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaUdpEndpoints {
    pub host: IpAddr,
    pub video1_port: u16,
    pub video2_port: Option<u16>,
    pub audio_port: Option<u16>,
    remote_port_overrides: MediaUdpPortOverrides,
}

impl MediaUdpEndpoints {
    /// Build endpoints from a confirmed `MediaStreamMessage1`.
    pub fn from_message1(host: IpAddr, message: &MediaStreamMessage1) -> Self {
        Self {
            host,
            video1_port: message.video1_port,
            video2_port: message.video2_port,
            audio_port: message.audio_port,
            remote_port_overrides: MediaUdpPortOverrides::default(),
        }
    }

    /// Apply external destination ports without changing the local ports
    /// advertised by Screen Sharing.
    pub fn with_remote_port_overrides(mut self, overrides: MediaUdpPortOverrides) -> Result<Self> {
        overrides.validate()?;
        self.remote_port_overrides = overrides;
        Ok(self)
    }

    pub const fn remote_port_overrides(&self) -> MediaUdpPortOverrides {
        self.remote_port_overrides
    }

    pub fn port_for(&self, kind: UdpStreamKind) -> Option<u16> {
        match kind {
            UdpStreamKind::Video1 => Some(self.video1_port),
            UdpStreamKind::Video2 => self.video2_port,
            UdpStreamKind::Audio => self.audio_port,
        }
    }

    /// Remote destination after applying an optional port-forward override.
    pub fn remote_port_for(&self, kind: UdpStreamKind) -> Option<u16> {
        self.remote_port_overrides
            .port_for(kind)
            .or_else(|| self.port_for(kind))
    }
}

/// One connected UDP socket plus the per-stream SRTP context.
pub struct MediaUdpSession {
    socket: UdpSocket,
    remote: SocketAddr,
    kind: UdpStreamKind,
}

impl MediaUdpSession {
    /// Bind the negotiated port locally and connect to the remote endpoint.
    /// Screen Sharing uses the same port number on both sides so that the
    /// server can send the first RTP packet before receiving client traffic.
    /// A configured forwarding override changes only `remote`, never the
    /// local bind address.
    pub fn connect(endpoints: &MediaUdpEndpoints, kind: UdpStreamKind) -> Result<Self> {
        let local_port = endpoints
            .port_for(kind)
            .ok_or(Error::Invalid("endpoint not offered for stream kind"))?;
        let remote_port = endpoints
            .remote_port_for(kind)
            .ok_or(Error::Invalid("endpoint not offered for stream kind"))?;
        let remote = SocketAddr::new(endpoints.host, remote_port);
        let bind_addr = match endpoints.host {
            IpAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), local_port),
            IpAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), local_port),
        };
        // The native client enables both SO_REUSEADDR and SO_REUSEPORT before
        // binding the negotiated media port. Without them a reconnect, a second
        // stream on an overlapping port, or the sibling audio/video sockets
        // holding the port make `bind` fail with EADDRINUSE, which surfaces as
        // "UDP endpoint unavailable" and refuses to start the media path.
        //
        // `socket2` is unavailable on wasm, where this path is not used either,
        // so fall back to a plain bind there.
        #[cfg(not(target_arch = "wasm32"))]
        let socket = {
            let socket = socket2::Socket::new(
                match endpoints.host {
                    IpAddr::V4(_) => socket2::Domain::IPV4,
                    IpAddr::V6(_) => socket2::Domain::IPV6,
                },
                socket2::Type::DGRAM,
                Some(socket2::Protocol::UDP),
            )
            .map_err(io_error)?;
            socket.set_reuse_address(true).map_err(io_error)?;
            #[cfg(unix)]
            socket.set_reuse_port(true).map_err(io_error)?;
            socket.bind(&bind_addr.into()).map_err(io_error)?;
            UdpSocket::from(socket)
        };
        #[cfg(target_arch = "wasm32")]
        let socket = UdpSocket::bind(bind_addr).map_err(io_error)?;
        socket
            .set_read_timeout(Some(UDP_READ_TIMEOUT))
            .map_err(io_error)?;
        socket.connect(remote).map_err(io_error)?;
        Ok(Self {
            socket,
            remote,
            kind,
        })
    }

    pub fn remote(&self) -> SocketAddr {
        self.remote
    }

    pub fn kind(&self) -> UdpStreamKind {
        self.kind
    }

    /// Receive one datagram into `buf`; returns the number of bytes read.
    pub fn recv(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.socket.recv(buf)
    }

    /// Send a raw datagram (used for RTCP feedback or keep-alives).
    pub fn send(&self, bytes: &[u8]) -> std::io::Result<usize> {
        self.socket.send(bytes)
    }
}

/// Combination of UDP receive, SRTP decryption and RTP de-packetization for
/// one video stream. Drives `visit` once per completed access unit.
pub struct AvcVideoStreamReceiver {
    session: MediaUdpSession,
    /// Optional development dump of the media stream. It records each RTP packet
    /// after SRTP authentication and decryption and before RTP reassembly, so the
    /// dump holds the server's own packet without the session's keys and without
    /// the decoded picture.
    raw_stream: Option<Arc<RawStreamSink>>,
    raw_stream_kind: RawStreamKind,
    /// Whether a raw-stream take was armed on the previous packet, and whether
    /// that take has already carried a keyframe. A take that begins mid-chain
    /// needs one requested so the file can be decoded on its own.
    take_armed: bool,
    take_has_keyframe: bool,
    take_keyframe_requested: bool,
    /// Codec of the assembled units, for recognising a keyframe.
    codec: MediaStreamCodec,
    /// One SRTP context per band, in band order. Decryption stays here because
    /// it needs the session keys; ordering and assembly do not.
    crypto: Vec<SrtpContext>,
    /// The ordering and assembly machine, shared with the rebuild path so both
    /// reassemble the same packets the same way.
    assembler: VideoStreamAssembler,
    feedback: Vec<FeedbackStream>,
    expected_payload_type: u8,
    base_remote_ssrc: u32,
    buffer: Vec<u8>,
    decrypted_buffer: Vec<u8>,
    ready_units: VecDeque<(usize, AccessUnit)>,
    packets_received: usize,
    decrypted_packets: usize,
    heartbeats_sent: usize,
    frames: usize,
    packet_losses: usize,
    last_feedback: Instant,
    last_keyframe_request: Option<Instant>,
}

struct FeedbackStream {
    remote_ssrc: u32,
    srtcp: SrtcpContext,
    /// Decrypted payload bytes and the instant the window opened. The
    /// rate-control feedback carries what this receiver measured, so the window
    /// is closed and reopened every time a feedback packet is sent.
    window: ReceiveWindow,
    /// Loss and jitter for this stream's receiver report.
    stats: ReceptionStats,
}

/// One stream's receive counters for the feedback interval.
///
/// The rate-control feedback reports what this receiver measured *since the last
/// report*, because that is the shape the peer's reader compares: the native
/// builder takes its two per-interval figures from statistics the collector
/// clears as soon as they have been reported.
#[derive(Clone, Copy)]
struct ReceiveWindow {
    bytes: u64,
    datagrams: u32,
    opened_at: Instant,
}

/// One interval's measurement, taken and then reset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReceiveInterval {
    bits_per_second: u32,
    datagrams: u32,
}

impl ReceiveWindow {
    fn new(opened_at: Instant) -> Self {
        Self {
            bytes: 0,
            datagrams: 0,
            opened_at,
        }
    }

    fn record(&mut self, payload_bytes: usize) {
        self.bytes = self.bytes.saturating_add(payload_bytes as u64);
        self.datagrams = self.datagrams.saturating_add(1);
    }

    /// The window's rates, then restart it. A window that has not advanced
    /// reports zeros rather than dividing by a zero interval.
    fn close(&mut self, now: Instant) -> ReceiveInterval {
        let elapsed = now.saturating_duration_since(self.opened_at);
        let bytes = std::mem::replace(&mut self.bytes, 0);
        let datagrams = std::mem::replace(&mut self.datagrams, 0);
        self.opened_at = now;
        let micros = elapsed.as_micros();
        if micros == 0 {
            return ReceiveInterval {
                bits_per_second: 0,
                datagrams,
            };
        }
        let bits_per_second = (bytes as u128 * 8 * 1_000_000) / micros;
        ReceiveInterval {
            bits_per_second: u32::try_from(bits_per_second).unwrap_or(u32::MAX),
            datagrams,
        }
    }
}

/// One stream's RTCP reception statistics (RFC 3550 section 6.4.1).
///
/// The viewer's only channel for loss and jitter feedback is the receiver
/// report, so this tracks the same quantities the native receiver reports: the
/// extended highest sequence number, the packets received and expected since the
/// session began, and the interarrival jitter estimate of RFC 3550 A.8.
#[derive(Clone, Copy)]
struct ReceptionStats {
    started: bool,
    /// Wraps of the 16-bit sequence number, and its latest value.
    cycles: u32,
    highest: u16,
    /// The extended sequence the stream started at, so "expected" counts from
    /// the first packet rather than from the sequence counter's own origin.
    base: u32,
    received: u64,
    expected_prior: u64,
    received_prior: u64,
    jitter: f64,
    last_timestamp: u32,
    last_arrival: Instant,
}

/// One receiver report's counters, taken over the interval since the previous
/// report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReceptionReportValues {
    fraction_lost: u8,
    cumulative_lost: i32,
    highest_sequence: u32,
    jitter: u32,
}

/// The media stream's RTP clock, from `rtpmap ... HEVC/90000`.
const RTP_CLOCK_RATE: f64 = 90_000.0;

impl ReceptionStats {
    fn new() -> Self {
        Self {
            started: false,
            cycles: 0,
            highest: 0,
            base: 0,
            received: 0,
            expected_prior: 0,
            received_prior: 0,
            jitter: 0.0,
            last_timestamp: 0,
            last_arrival: Instant::now(),
        }
    }

    /// The RFC 3550 extended sequence number: cycles times 65536 plus the
    /// 16-bit sequence, which is the value a report carries.
    fn extended(&self) -> u32 {
        self.cycles
            .wrapping_mul(1 << 16)
            .wrapping_add(u32::from(self.highest))
    }

    fn record(&mut self, sequence: u16, timestamp: u32, arrival: Instant) {
        if !self.started {
            self.started = true;
            self.highest = sequence;
            self.base = self.extended();
            self.last_timestamp = timestamp;
            self.last_arrival = arrival;
            self.received = 1;
            return;
        }
        // A forward 16-bit delta advances the sequence; a forward delta that
        // moves the 16-bit value backwards means the counter wrapped.
        let delta = sequence.wrapping_sub(self.highest) as i16;
        if delta > 0 {
            if sequence < self.highest {
                self.cycles = self.cycles.wrapping_add(1);
            }
            self.highest = sequence;
        }
        self.received = self.received.saturating_add(1);

        let arrival_delta = arrival
            .saturating_duration_since(self.last_arrival)
            .as_secs_f64();
        let timestamp_delta = timestamp.wrapping_sub(self.last_timestamp) as i32 as f64;
        let transit = timestamp_delta - arrival_delta * RTP_CLOCK_RATE;
        self.jitter += (transit.abs() - self.jitter) / 16.0;
        self.last_timestamp = timestamp;
        self.last_arrival = arrival;
    }

    /// This interval's counters, then roll the interval forward.
    fn report(&mut self) -> ReceptionReportValues {
        let highest_sequence = self.extended();
        let expected = u64::from(highest_sequence.wrapping_sub(self.base)).saturating_add(1);
        let expected_interval = expected.saturating_sub(self.expected_prior);
        let received_interval = self.received.saturating_sub(self.received_prior);
        self.expected_prior = expected;
        self.received_prior = self.received;

        let lost_interval = expected_interval.saturating_sub(received_interval);
        let fraction_lost = (lost_interval * 256)
            .checked_div(expected_interval)
            .map_or(0, |value| {
                u8::try_from(value.min(255)).expect("fraction lost fits a byte")
            });
        let cumulative = i64::try_from(expected).unwrap_or(i64::MAX)
            - i64::try_from(self.received).unwrap_or(i64::MAX);
        ReceptionReportValues {
            fraction_lost,
            cumulative_lost: cumulative.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
            highest_sequence,
            jitter: self.jitter.clamp(0.0, f64::from(u32::MAX)) as u32,
        }
    }
}

/// Whether the development switch that silences the viewer's feedback packets
/// is set (`ARD_MEDIA_FEEDBACK=off`). A take can then be compared with and
/// without them, which is the only way to see what the server's rate control
/// does about them.
fn feedback_disabled() -> bool {
    matches!(
        std::env::var("ARD_MEDIA_FEEDBACK").ok().as_deref(),
        Some("off") | Some("0") | Some("false")
    )
}

/// Borrowed session credentials for one bidirectional AVC media stream.
pub struct AvcStreamCrypto<'a> {
    pub server_to_viewer_key_blob: &'a [u8],
    pub viewer_to_server_key_blob: &'a [u8],
    pub remote_ssrc: u32,
    pub local_ssrc: u32,
}

/// The horizontal bands sharing one RTP sampling instant, as a live receiver
/// reports them. Ordering is the assembler's; this is the shaped result the
/// decoder pipeline consumes.
#[derive(Debug)]
pub struct AvcFrameBatch {
    pub timestamp: u32,
    pub access_units: Vec<(usize, AccessUnit)>,
    /// Local monotonic time immediately after the first UDP datagram for any
    /// access unit in this sampling instant was received.
    pub first_packet_received_at: Instant,
    /// Local monotonic time at which the first complete access unit finished
    /// RTP reassembly.
    pub first_access_unit_completed_at: Instant,
    /// Local monotonic time at which the batch was released to the decoder.
    pub released_at: Instant,
}

impl AvcVideoStreamReceiver {
    pub fn new(
        endpoints: &MediaUdpEndpoints,
        kind: UdpStreamKind,
        crypto: AvcStreamCrypto<'_>,
        codec: MediaStreamCodec,
        payload_type: u8,
        raw_stream: Option<Arc<RawStreamSink>>,
    ) -> Result<Self> {
        let session = MediaUdpSession::connect(endpoints, kind)?;
        let mut crypto_contexts = Vec::with_capacity(AVC_VIDEO_SLICE_COUNT);
        let mut feedback = Vec::with_capacity(AVC_VIDEO_SLICE_COUNT);
        for layer in 0..AVC_VIDEO_SLICE_COUNT as u32 {
            let remote_ssrc = crypto.remote_ssrc.wrapping_add(layer);
            let local_ssrc = crypto.local_ssrc.wrapping_add(layer);
            crypto_contexts.push(SrtpContext::from_key_blob_with_derived_ssrc(
                crypto.server_to_viewer_key_blob,
                remote_ssrc,
            )?);
            feedback.push(FeedbackStream {
                remote_ssrc,
                srtcp: SrtcpContext::from_key_blob_with_sender_ssrc(
                    crypto.viewer_to_server_key_blob,
                    local_ssrc,
                )?,
                window: ReceiveWindow::new(Instant::now()),
                stats: ReceptionStats::new(),
            });
        }
        // The assembler is told the band order up front, so a batch's decode
        // order maps onto the decoder's slice indices the same way a rebuild's
        // does.
        let mut assembler = VideoStreamAssembler::new(codec);
        for layer in 0..AVC_VIDEO_SLICE_COUNT as u32 {
            assembler.expect_stream(crypto.remote_ssrc.wrapping_add(layer));
        }
        let raw_stream_kind = match kind {
            UdpStreamKind::Video1 => RawStreamKind::Video,
            UdpStreamKind::Video2 => RawStreamKind::Video2,
            UdpStreamKind::Audio => RawStreamKind::Audio,
        };
        let mut receiver = Self {
            session,
            raw_stream,
            raw_stream_kind,
            take_armed: false,
            take_has_keyframe: false,
            take_keyframe_requested: false,
            codec,
            crypto: crypto_contexts,
            assembler,
            feedback,
            expected_payload_type: payload_type,
            base_remote_ssrc: crypto.remote_ssrc,
            buffer: vec![0u8; MAX_RTP_PACKET],
            decrypted_buffer: Vec::with_capacity(MAX_RTP_PACKET),
            ready_units: VecDeque::new(),
            packets_received: 0,
            decrypted_packets: 0,
            heartbeats_sent: 0,
            frames: 0,
            packet_losses: 0,
            last_feedback: Instant::now(),
            last_keyframe_request: None,
        };
        receiver.send_initial_heartbeats()?;
        Ok(receiver)
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    pub fn packets_received(&self) -> usize {
        self.packets_received
    }

    pub fn decrypted_packets(&self) -> usize {
        self.decrypted_packets
    }

    pub fn heartbeats_sent(&self) -> usize {
        self.heartbeats_sent
    }

    pub fn packet_losses(&self) -> usize {
        self.packet_losses
    }

    /// Compatibility access-unit API. Units are still released in complete
    /// RTP-timestamp batches, so callers cannot decode a newer desktop frame
    /// before a late band from the preceding sampling instant.
    pub fn receive(&mut self) -> Result<Option<(usize, AccessUnit)>> {
        if let Some(unit) = self.ready_units.pop_front() {
            return Ok(Some(unit));
        }
        if let Some(batch) = self.receive_frame()? {
            self.ready_units.extend(batch.access_units);
        }
        Ok(self.ready_units.pop_front())
    }

    /// Receive one complete desktop sampling instant. Native AVC can omit
    /// unchanged bands, but every submitted access unit must remain in the
    /// global DON/DONL sequence. A later DON proves a sparse timestamp's end;
    /// wall-clock expiry never does. Missing sequence state is bounded by
    /// `MAX_PENDING_FRAME_BATCHES`, then recovered with an explicit PLI and
    /// fresh codec sync frame.
    pub fn receive_frame(&mut self) -> Result<Option<AvcFrameBatch>> {
        self.send_feedback_if_due()?;
        if let Some(batch) = self.take_ready_frame() {
            return Ok(Some(batch));
        }
        let len = match self.session.recv(&mut self.buffer) {
            Ok(len) => len,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Ok(self.take_ready_frame());
            }
            Err(_) => return Err(Error::Invalid("RTP receive failed")),
        };
        let packet_received_at = Instant::now();
        if len > MAX_RTP_PACKET {
            return Err(Error::LimitExceeded("RTP datagram"));
        }
        if is_rtcp(&self.buffer[..len]) {
            return Ok(None);
        }
        if len <= super::srtp::AUTH_TAG_LEN {
            return Err(Error::Invalid("SRTP RTP packet is too short"));
        }
        let body_len = len - super::srtp::AUTH_TAG_LEN;
        let mut authentication_tag = [0u8; super::srtp::AUTH_TAG_LEN];
        authentication_tag.copy_from_slice(&self.buffer[body_len..len]);
        let (sequence, timestamp, payload_offset, payload_type, ssrc) = {
            let encrypted_packet = RtpPacket::parse_encrypted(&self.buffer[..body_len])?;
            (
                encrypted_packet.header.sequence,
                encrypted_packet.header.timestamp,
                encrypted_packet.payload_offset,
                encrypted_packet.header.payload_type,
                encrypted_packet.header.ssrc,
            )
        };
        // Native AVC advertises one base SSRC but sends four adjacent vertical
        // desktop slices, each with independent sequence/replay state and a
        // matching local feedback SSRC.
        let stream_index = ssrc.wrapping_sub(self.base_remote_ssrc) as usize;
        let Some(context) = self.crypto.get_mut(stream_index) else {
            return Ok(None);
        };
        self.packets_received += 1;
        if payload_type != self.expected_payload_type {
            return Err(Error::Invalid("unexpected negotiated RTP payload type"));
        }
        self.decrypted_buffer.clear();
        self.decrypted_buffer
            .extend_from_slice(&self.buffer[..body_len]);
        context.decrypt_authenticated_rtp_packet_in_place(
            &mut self.decrypted_buffer,
            &authentication_tag,
            sequence,
            payload_offset,
        )?;
        self.decrypted_packets += 1;
        // Both feedback channels report what this receiver measured: the
        // rate-control packet carries the interval's datagrams and bytes, and
        // the receiver report carries the loss and jitter counters.
        if let Some(feedback) = self.feedback.get_mut(stream_index) {
            feedback.window.record(self.decrypted_buffer.len());
            feedback
                .stats
                .record(sequence, timestamp, packet_received_at);
        }
        // The dump is taken here: the packet has cleared SRTP authentication and
        // its payload is decrypted, and the RTP assembler has not touched it.
        // A rebuild can therefore replay the same packet without the session's
        // SRTP keys, which are never written anywhere. A control datagram never
        // reaches this point, so the dump holds RTP packets and nothing else.
        if let Some(sink) = self.raw_stream.as_ref() {
            sink.record_udp(
                self.raw_stream_kind,
                self.packets_received.saturating_sub(1) as u32,
                &self.decrypted_buffer,
                sequence,
                ssrc,
            );
        }
        // A take that starts mid-session captures a chain whose keyframe is
        // already behind it, and the recording can then never be replayed on its
        // own: the assembler waits for an origin that is not in the file. While
        // a take is armed and has not carried a keyframe yet, ask the server for
        // one. The request is rate limited with the recovery path's own
        // interval, and a take that begins on the session's first keyframe needs
        // no request at all.
        let armed = self
            .raw_stream
            .as_ref()
            .is_some_and(|sink| sink.writes_a_take());
        if armed != self.take_armed {
            self.take_armed = armed;
            self.take_has_keyframe = false;
            self.take_keyframe_requested = false;
        }
        if armed && !self.take_has_keyframe && !self.take_keyframe_requested {
            // Ask once per take. The request goes out without dropping the
            // live chain: the recording needs a keyframe among its packets,
            // while the picture on screen is already decoding fine.
            self.take_keyframe_requested = true;
            self.send_keyframe_request()?;
        }
        // Ordering, batching and sync recovery are the assembler's, so a rebuild
        // that replays these same packets reassembles them identically.
        let pushed =
            self.assembler
                .push_packet(ssrc, &self.decrypted_buffer, packet_received_at)?;
        if pushed.losses != 0 {
            self.packet_losses = self.packet_losses.saturating_add(pushed.losses);
            if std::env::var_os("ARD_MEDIA_TRACE").is_some() {
                eprintln!(
                    "RTP loss: stream={stream_index} timestamp={timestamp} sequence={sequence} dropped_access_units={} total={}",
                    pushed.losses, self.packet_losses,
                );
            }
            self.send_keyframe_request()?;
        }
        let assembled = self.assembler.receive()?;
        if assembled.chain_reset {
            self.packet_losses = self.packet_losses.saturating_add(1);
            self.send_keyframe_request()?;
        }
        if assembled.ignored_units != 0 && std::env::var_os("ARD_MEDIA_TRACE").is_some() {
            eprintln!(
                "RTP ignored completed late/duplicate access units: count={}",
                assembled.ignored_units,
            );
        }
        let codec = self.codec;
        if let Some(frame) = assembled.frame.as_ref()
            && self.take_armed
            && frame
                .access_units
                .iter()
                .any(|(_, unit)| unit.is_sync(codec))
        {
            self.take_has_keyframe = true;
        }
        Ok(assembled.frame.map(|frame| {
            self.frames = self.frames.saturating_add(1);
            AvcFrameBatch {
                timestamp: frame.timestamp,
                access_units: frame.access_units,
                first_packet_received_at: frame.first_packet_received_at,
                first_access_unit_completed_at: frame.first_access_unit_completed_at,
                released_at: Instant::now(),
            }
        }))
    }

    /// Release a frame the assembler already has ready without reading a packet.
    fn take_ready_frame(&mut self) -> Option<AvcFrameBatch> {
        let assembled = self.assembler.receive().ok()?;
        let codec = self.codec;
        if let Some(frame) = assembled.frame.as_ref()
            && self.take_armed
            && frame
                .access_units
                .iter()
                .any(|(_, unit)| unit.is_sync(codec))
        {
            self.take_has_keyframe = true;
        }
        assembled.frame.map(|frame| {
            self.frames = self.frames.saturating_add(1);
            AvcFrameBatch {
                timestamp: frame.timestamp,
                access_units: frame.access_units,
                first_packet_received_at: frame.first_packet_received_at,
                first_access_unit_completed_at: frame.first_access_unit_completed_at,
                released_at: Instant::now(),
            }
        })
    }

    fn send_feedback_if_due(&mut self) -> Result<()> {
        if self.last_feedback.elapsed() >= RTCP_REPORT_INTERVAL {
            self.send_receiver_reports()?;
        }
        Ok(())
    }

    fn send_initial_heartbeats(&mut self) -> Result<()> {
        for feedback in &mut self.feedback {
            let heartbeat = feedback.srtcp.protect_heartbeat()?;
            self.session.send(&heartbeat).map_err(io_error)?;
            self.heartbeats_sent += 1;
        }
        self.last_feedback = Instant::now();
        Ok(())
    }

    /// One rate-control feedback packet per received stream.
    ///
    /// The server's adaptive rate control runs on what the viewer echoes back,
    /// so the packet carries the interval this receiver actually measured — the
    /// datagrams and payload bytes since the previous report (see
    /// `protect_rate_control_feedback` for the packet's shape and why its width
    /// matters).
    fn send_receiver_reports(&mut self) -> Result<()> {
        let now = Instant::now();
        if feedback_disabled() {
            // A/B switch for a take that has to run without the viewer's
            // feedback: the windows still roll so the counters stay honest if
            // the next report sends them.
            for feedback in &mut self.feedback {
                let _ = feedback.window.close(now);
                let _ = feedback.stats.report();
            }
            self.last_feedback = now;
            return Ok(());
        }
        for feedback in &mut self.feedback {
            let interval = feedback.window.close(now);
            let rate_control = feedback.srtcp.protect_rate_control_feedback(
                feedback.remote_ssrc,
                interval.bits_per_second,
                interval.datagrams,
            )?;
            self.session.send(&rate_control).map_err(io_error)?;
            self.heartbeats_sent += 1;

            let values = feedback.stats.report();
            let report = feedback.srtcp.protect_receiver_report(
                feedback.remote_ssrc,
                values.fraction_lost,
                values.cumulative_lost,
                values.highest_sequence,
                values.jitter,
            )?;
            self.session.send(&report).map_err(io_error)?;
            self.heartbeats_sent += 1;
        }
        self.last_feedback = now;
        Ok(())
    }

    pub fn request_keyframe(&mut self) -> Result<()> {
        self.assembler.enter_sync_recovery();
        self.ready_units.clear();
        self.send_keyframe_request()
    }

    /// Send picture-loss indications for every received stream, rate limited to
    /// one request per `KEYFRAME_REQUEST_INTERVAL`.
    pub fn send_keyframe_request(&mut self) -> Result<()> {
        if self
            .last_keyframe_request
            .is_some_and(|last| last.elapsed() < KEYFRAME_REQUEST_INTERVAL)
        {
            return Ok(());
        }
        for feedback in &mut self.feedback {
            let request = feedback
                .srtcp
                .protect_picture_loss_indication(feedback.remote_ssrc)?;
            self.session.send(&request).map_err(io_error)?;
            self.heartbeats_sent += 1;
        }
        self.last_keyframe_request = Some(Instant::now());
        Ok(())
    }
}

fn is_rtcp(datagram: &[u8]) -> bool {
    datagram.len() >= 4 && datagram[0] >> 6 == 2 && (192..=223).contains(&datagram[1])
}

fn io_error(error: std::io::Error) -> Error {
    Error::Invalid(if error.kind() == std::io::ErrorKind::AddrNotAvailable {
        "UDP endpoint unavailable"
    } else {
        "UDP socket error"
    })
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, UdpSocket};
    use std::time::Duration;

    use super::{
        AVC_VIDEO_SLICE_COUNT, MediaUdpEndpoints, MediaUdpPortOverrides, MediaUdpSession,
        ReceiveInterval, ReceiveWindow, ReceptionStats, UdpStreamKind, is_rtcp,
    };
    use crate::media_stream::{ENCODING_AVC_MEDIA_STREAM, MediaStreamMessage1};

    /// The feedback the server's rate control consumes is this receiver's own
    /// measurement, so the window has to report the interval it measured and
    /// then start over: the peer compares consecutive reports, and a stale or
    /// repeated figure would describe a rate the viewer never saw.
    #[test]
    fn receive_window_reports_and_resets_the_interval_counters() {
        let opened = std::time::Instant::now();
        let mut window = ReceiveWindow::new(opened);
        for _ in 0..1_000 {
            window.record(2_000);
        }
        // 2 MB in one second is 16 Mbit/s, over 1000 datagrams.
        assert_eq!(
            window.close(opened + Duration::from_secs(1)),
            ReceiveInterval {
                bits_per_second: 16_000_000,
                datagrams: 1_000,
            }
        );

        // The next interval starts empty, so a stream that stops arriving
        // reports zero instead of repeating the last figures.
        assert_eq!(
            window.close(opened + Duration::from_secs(2)),
            ReceiveInterval {
                bits_per_second: 0,
                datagrams: 0,
            }
        );

        // A window that has not advanced cannot divide by a zero interval, but
        // the datagrams it did see are still reported.
        window.record(1_000);
        assert_eq!(
            window.close(opened + Duration::from_secs(2)),
            ReceiveInterval {
                bits_per_second: 0,
                datagrams: 1,
            }
        );
    }

    /// The receiver report is the only loss and jitter feedback the viewer
    /// sends, so its counters have to describe the interval since the previous
    /// report: a missing sequence is one lost packet, and a stream that then
    /// goes quiet must not repeat the earlier fraction.
    #[test]
    fn reception_stats_report_loss_over_the_interval() {
        let start = std::time::Instant::now();
        let mut stats = ReceptionStats::new();
        // Sequences 1 and 2 arrive, 3 is lost, 4 arrives.
        for (index, sequence) in [(0u64, 1u16), (1, 2), (3, 4)] {
            stats.record(
                sequence,
                90_000 * index as u32,
                start + Duration::from_millis(index),
            );
        }
        let first = stats.report();
        assert_eq!(
            first.highest_sequence, 4,
            "the extended sequence is reported"
        );
        assert_eq!(
            first.cumulative_lost, 1,
            "one of four expected packets is lost"
        );
        // One lost of four expected is 64/256.
        assert_eq!(first.fraction_lost, 64);

        // A quiet interval adds nothing expected and nothing received.
        let second = stats.report();
        assert_eq!(second.fraction_lost, 0);
        assert_eq!(second.cumulative_lost, 1);
        assert_eq!(second.highest_sequence, 4);

        // The sequence counter wrapping is a cycle, not a jump backwards: the
        // extended number keeps counting and a contiguous wrap loses nothing.
        let mut wrapping = ReceptionStats::new();
        wrapping.record(65_535, 0, start);
        wrapping.record(0, 90_000, start + Duration::from_millis(1));
        let values = wrapping.report();
        assert_eq!(values.highest_sequence, 65_536);
        assert_eq!(values.cumulative_lost, 0);
    }

    /// A steady stream with a constant transit time has no jitter, and a stream
    /// whose arrival swings away from its timestamps reports the swing.
    #[test]
    fn reception_stats_track_interarrival_jitter() {
        let start = std::time::Instant::now();
        let mut steady = ReceptionStats::new();
        for index in 0..20u64 {
            steady.record(
                index as u16,
                (90_000 * index / 10) as u32,
                start + Duration::from_millis(index * 100),
            );
        }
        assert_eq!(steady.report().jitter, 0);

        let mut swinging = ReceptionStats::new();
        swinging.record(0, 0, start);
        // 10 ms late for a 10 ms step, so the estimated variation is 900 units.
        swinging.record(1, 900, start + Duration::from_millis(20));
        assert!(swinging.report().jitter > 0);
    }

    #[test]
    fn distinguishes_rtcp_from_rtp() {
        assert!(is_rtcp(&[0x80, 200, 0, 1]));
        assert!(is_rtcp(&[0x80, 206, 0, 1]));
        assert!(!is_rtcp(&[0x80, 96, 0, 1]));
        assert!(!is_rtcp(&[0x40, 200, 0, 1]));
    }

    #[test]
    fn native_desktop_uses_four_video_slices() {
        assert_eq!(AVC_VIDEO_SLICE_COUNT, 4);
    }

    #[test]
    fn forwarded_destination_keeps_the_negotiated_local_bind_port() {
        let local_reservation = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("local port");
        let local_port = local_reservation
            .local_addr()
            .expect("local address")
            .port();
        drop(local_reservation);

        let remote = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("remote port");
        remote
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("read timeout");
        let remote_port = remote.local_addr().expect("remote address").port();
        assert_ne!(local_port, remote_port);

        let message = MediaStreamMessage1 {
            encoding: ENCODING_AVC_MEDIA_STREAM,
            video1_port: local_port,
            video2_port: Some(5902),
            audio_port: Some(5900),
            video1_hdr: false,
            video2_hdr: false,
            stream_count: 1,
        };
        let endpoints = MediaUdpEndpoints::from_message1(IpAddr::V4(Ipv4Addr::LOCALHOST), &message)
            .with_remote_port_overrides(MediaUdpPortOverrides {
                video1: Some(remote_port),
                ..MediaUdpPortOverrides::default()
            })
            .expect("valid override");
        let session =
            MediaUdpSession::connect(&endpoints, UdpStreamKind::Video1).expect("forwarded session");

        session.send(b"forwarded").expect("send datagram");
        let mut buffer = [0_u8; 32];
        let (len, source) = remote.recv_from(&mut buffer).expect("receive datagram");
        assert_eq!(&buffer[..len], b"forwarded");
        assert_eq!(source.port(), local_port);
        assert_eq!(session.remote().port(), remote_port);
        assert_eq!(endpoints.port_for(UdpStreamKind::Video1), Some(local_port));
        assert_eq!(
            endpoints.remote_port_for(UdpStreamKind::Video1),
            Some(remote_port)
        );
        assert_eq!(endpoints.remote_port_for(UdpStreamKind::Audio), Some(5900));
        assert_eq!(endpoints.remote_port_for(UdpStreamKind::Video2), Some(5902));
    }

    #[test]
    fn remote_port_overrides_reject_zero() {
        assert!(
            MediaUdpPortOverrides {
                video1: Some(0),
                ..MediaUdpPortOverrides::default()
            }
            .validate()
            .is_err()
        );
    }
}
