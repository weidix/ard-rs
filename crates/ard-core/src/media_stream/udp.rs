//! UDP transport for Apple's real-time media stream.
//!
//! The server hands out media ports in `MediaStreamMessage1`; the client
//! binds the same local port per stream and connects to the explicit remote
//! endpoint for each stream (video1 is base+1 on the confirmed build).
//! Packets are standard RTP (RFC 3550) with Apple's AVC SRTP payload
//! encryption and suite-5 authentication suffixes.

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use crate::{Error, Result};

use super::MAX_RTP_PACKET;
use super::negotiation::MediaStreamCodec;
use super::rtp::{AccessUnit, H264Depacketizer, HevcDepacketizer, RtpPacket, RtpReorderBuffer};
use super::srtp::{SrtcpContext, SrtpContext};
use super::wire::MediaStreamMessage1;

const RTCP_REPORT_INTERVAL: Duration = Duration::from_secs(1);
const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_millis(100);
const UDP_READ_TIMEOUT: Duration = Duration::from_millis(10);
/// Bound cross-SSRC decoding-order reassembly. This is a loss detector, not a
/// presentation timer: a sparse timestamp is released only when the following
/// DON/DONL proves its exact boundary.
const MAX_PENDING_FRAME_BATCHES: usize = 8;
const MAX_TRACKED_RTP_TIMESTAMPS: usize = 64;
/// Native AVC partitions one desktop frame into four horizontal slices. Each
/// slice uses an adjacent SSRC with independent SRTP/RTP state, while all four
/// share one interleaved video decoder reference chain.
pub const AVC_VIDEO_SLICE_COUNT: usize = 4;
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
    streams: Vec<InboundVideoStream>,
    feedback: Vec<FeedbackStream>,
    codec: MediaStreamCodec,
    awaiting_sync: bool,
    pending_sync_followers: Vec<(usize, TimedAccessUnit)>,
    expected_payload_type: u8,
    base_remote_ssrc: u32,
    buffer: Vec<u8>,
    decrypted_buffer: Vec<u8>,
    frame_batcher: AccessUnitBatcher,
    ready_units: VecDeque<(usize, AccessUnit)>,
    packets_received: usize,
    decrypted_packets: usize,
    heartbeats_sent: usize,
    frames: usize,
    packet_losses: usize,
    last_feedback: Instant,
    last_keyframe_request: Option<Instant>,
}

/// The horizontal bands sharing one RTP sampling instant. RFC 3550 defines
/// the RTP timestamp as the sampling instant; Apple's four adjacent SSRCs
/// reuse it for one serial prediction chain. Unchanged bands can be omitted,
/// so DON/DONL—not elapsed time or SSRC scan order—defines the batch boundary
/// and decoder submission order.
#[derive(Debug)]
pub struct AvcFrameBatch {
    pub timestamp: u32,
    pub access_units: Vec<(usize, AccessUnit)>,
    /// Local monotonic time immediately after the first UDP datagram for any
    /// access unit in this sampling instant was received.
    pub first_packet_received_at: Instant,
    /// Local monotonic time at which the first complete access unit in this
    /// sampling instant finished RTP reassembly.
    pub first_access_unit_completed_at: Instant,
    /// Local monotonic time at which the complete/bounded batch was released
    /// to the platform decoder.
    pub released_at: Instant,
}

struct PendingFrameBatch {
    timestamp: u32,
    first_packet_received_at: Instant,
    first_access_unit_completed_at: Instant,
    access_units: Vec<(usize, AccessUnit)>,
}

#[derive(Debug)]
struct TimedAccessUnit {
    unit: AccessUnit,
    first_packet_received_at: Instant,
    completed_at: Instant,
}

#[derive(Default)]
struct AccessUnitBatcher {
    pending: Vec<PendingFrameBatch>,
    initial_decode_order: Option<u16>,
    last_released_decode_order: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchInsertResult {
    Accepted,
    IgnoredLate,
    MissingDecodeOrder,
    InvalidPredictionChain,
    PredictionChainOverflow,
}

impl AccessUnitBatcher {
    fn begin_prediction_chain(&mut self, decode_order: u16) {
        self.pending.clear();
        self.initial_decode_order = Some(decode_order);
        self.last_released_decode_order = None;
    }

    fn insert(&mut self, stream_index: usize, timed: TimedAccessUnit) -> BatchInsertResult {
        let TimedAccessUnit {
            unit,
            first_packet_received_at,
            completed_at,
        } = timed;
        let Some(decode_order) = unit.decode_order_number else {
            return BatchInsertResult::MissingDecodeOrder;
        };
        if self
            .last_released_decode_order
            .is_some_and(|released| !decode_order_is_newer(decode_order, released))
            || self.pending.iter().any(|batch| {
                batch
                    .access_units
                    .iter()
                    .any(|(_, pending)| pending.decode_order_number == Some(decode_order))
            })
        {
            return BatchInsertResult::IgnoredLate;
        }
        if let Some(batch) = self
            .pending
            .iter_mut()
            .find(|batch| batch.timestamp == unit.timestamp)
        {
            if batch
                .access_units
                .iter()
                .any(|(index, _)| *index == stream_index)
                || batch.access_units.len() >= AVC_VIDEO_SLICE_COUNT
            {
                return BatchInsertResult::InvalidPredictionChain;
            }
            batch.first_packet_received_at =
                batch.first_packet_received_at.min(first_packet_received_at);
            batch.first_access_unit_completed_at =
                batch.first_access_unit_completed_at.min(completed_at);
            batch.access_units.push((stream_index, unit));
            return BatchInsertResult::Accepted;
        }

        if self.pending.len() >= MAX_PENDING_FRAME_BATCHES {
            // A missing DON has held more than the bounded number of sampling
            // instants. Drop the dependent chain and wait for an explicit
            // codec sync frame after requesting PLI.
            if std::env::var_os("ARD_MEDIA_TRACE").is_some() {
                let layout = self
                    .pending
                    .iter()
                    .map(|batch| {
                        (
                            batch.timestamp,
                            batch
                                .access_units
                                .iter()
                                .map(|(index, unit)| (*index, unit.decode_order_number))
                                .collect::<Vec<_>>(),
                        )
                    })
                    .collect::<Vec<_>>();
                eprintln!(
                    "RTP pending prediction chain overflow before stream={stream_index} timestamp={}: {layout:?}",
                    unit.timestamp,
                );
            }
            self.pending.clear();
            self.initial_decode_order = None;
            self.last_released_decode_order = None;
            return BatchInsertResult::PredictionChainOverflow;
        }
        let position = self
            .pending
            .iter()
            .position(|batch| timestamp_precedes(unit.timestamp, batch.timestamp))
            .unwrap_or(self.pending.len());
        self.pending.insert(
            position,
            PendingFrameBatch {
                timestamp: unit.timestamp,
                first_packet_received_at,
                first_access_unit_completed_at: completed_at,
                access_units: vec![(stream_index, unit)],
            },
        );
        BatchInsertResult::Accepted
    }

    fn take_ready(&mut self, now: Instant) -> Option<AvcFrameBatch> {
        let (batch_index, first_decode_order) =
            if let Some(last_decode_order) = self.last_released_decode_order {
                let expected = last_decode_order.wrapping_add(1);
                let batch_index = self.pending.iter().position(|batch| {
                    batch
                        .access_units
                        .iter()
                        .any(|(_, unit)| unit.decode_order_number == Some(expected))
                })?;
                let batch = &self.pending[batch_index];
                let mut offsets = batch
                    .access_units
                    .iter()
                    .map(|(_, unit)| {
                        unit.decode_order_number
                            .expect("batcher accepts only access units carrying DON/DONL")
                            .wrapping_sub(expected)
                    })
                    .collect::<Vec<_>>();
                offsets.sort_unstable();
                if !offsets
                    .iter()
                    .enumerate()
                    .all(|(index, offset)| usize::from(*offset) == index)
                {
                    return None;
                }
                let following_decode_order = expected.wrapping_add(batch.access_units.len() as u16);
                let boundary_proven = batch.access_units.len() == AVC_VIDEO_SLICE_COUNT
                    || self.pending.iter().enumerate().any(|(index, following)| {
                        index != batch_index
                            && following.access_units.iter().any(|(_, unit)| {
                                unit.decode_order_number == Some(following_decode_order)
                            })
                    });
                if !boundary_proven {
                    return None;
                }
                (batch_index, expected)
            } else {
                // A clean native AVC chain begins with one full four-band sync
                // timestamp. Do not infer an initial sparse boundary without
                // a preceding decoding-order number.
                let expected = self.initial_decode_order?;
                let batch_index = self.pending.iter().position(|batch| {
                    batch
                        .access_units
                        .iter()
                        .any(|(_, unit)| unit.decode_order_number == Some(expected))
                })?;
                let batch = &self.pending[batch_index];
                if batch.access_units.len() != AVC_VIDEO_SLICE_COUNT
                    || !decode_orders_are_contiguous(&batch.access_units, expected)
                {
                    return None;
                }
                (batch_index, expected)
            };

        let mut batch = self.pending.remove(batch_index);
        batch.access_units.sort_by_key(|(_, unit)| {
            unit.decode_order_number
                .expect("batcher accepts only access units carrying DON/DONL")
                .wrapping_sub(first_decode_order)
        });
        self.last_released_decode_order = batch
            .access_units
            .last()
            .and_then(|(_, unit)| unit.decode_order_number);
        Some(AvcFrameBatch {
            timestamp: batch.timestamp,
            access_units: batch.access_units,
            first_packet_received_at: batch.first_packet_received_at,
            first_access_unit_completed_at: batch.first_access_unit_completed_at,
            released_at: now,
        })
    }

    fn reset(&mut self) {
        self.pending.clear();
        self.initial_decode_order = None;
        self.last_released_decode_order = None;
    }
}

fn decode_orders_are_contiguous(access_units: &[(usize, AccessUnit)], start: u16) -> bool {
    (0..access_units.len()).all(|offset| {
        access_units
            .iter()
            .any(|(_, unit)| unit.decode_order_number == Some(start.wrapping_add(offset as u16)))
    })
}

fn decode_order_is_newer(candidate: u16, reference: u16) -> bool {
    candidate != reference && candidate.wrapping_sub(reference) < 0x8000
}

enum VideoDepacketizer {
    H264(H264Depacketizer),
    Hevc(HevcDepacketizer),
}

impl VideoDepacketizer {
    fn new(codec: MediaStreamCodec) -> Self {
        match codec {
            MediaStreamCodec::H264 => Self::H264(H264Depacketizer::new()),
            MediaStreamCodec::Hevc => Self::Hevc(HevcDepacketizer::new_with_donl()),
        }
    }

    fn push(&mut self, packet: &RtpPacket<'_>) -> Result<Option<AccessUnit>> {
        match self {
            Self::H264(depacketizer) => depacketizer.push(packet),
            Self::Hevc(depacketizer) => depacketizer.push(packet),
        }
    }

    fn reset(&mut self) {
        match self {
            Self::H264(depacketizer) => depacketizer.reset(),
            Self::Hevc(depacketizer) => depacketizer.reset(),
        }
    }
}

struct InboundVideoStream {
    srtp: SrtpContext,
    reorder: RtpReorderBuffer,
    depacketizer: VideoDepacketizer,
    completed: VecDeque<TimedAccessUnit>,
    first_packet_arrivals: VecDeque<(u32, Instant)>,
    next_sequence: Option<u16>,
    damaged_timestamp: Option<u32>,
}

impl InboundVideoStream {
    fn new(key_blob: &[u8], ssrc: u32, codec: MediaStreamCodec) -> Result<Self> {
        Ok(Self {
            srtp: SrtpContext::from_key_blob_with_derived_ssrc(key_blob, ssrc)?,
            reorder: RtpReorderBuffer::with_codec(codec),
            depacketizer: VideoDepacketizer::new(codec),
            completed: VecDeque::new(),
            first_packet_arrivals: VecDeque::new(),
            next_sequence: None,
            damaged_timestamp: None,
        })
    }

    fn push_decrypted_packet(&mut self, packet: &[u8], received_at: Instant) -> Result<usize> {
        let packet_timestamp = RtpPacket::parse(packet)?.header.timestamp;
        if !self
            .first_packet_arrivals
            .iter()
            .any(|(timestamp, _)| *timestamp == packet_timestamp)
        {
            if self.first_packet_arrivals.len() >= MAX_TRACKED_RTP_TIMESTAMPS {
                return Err(Error::LimitExceeded("RTP timestamp timing tracker"));
            }
            self.first_packet_arrivals
                .push_back((packet_timestamp, received_at));
        }
        let ready_packets = self.reorder.push(packet)?;
        let mut losses = self.reorder.take_dropped_access_units();
        if losses != 0 {
            self.depacketizer.reset();
            self.damaged_timestamp = None;
            // The reorder buffer discarded whole stale timestamps. Resume at
            // the first packet of the intact newer burst instead of counting
            // that already-accounted gap a second time and discarding the
            // recovery frame as well.
            self.next_sequence = ready_packets.first().and_then(|packet| {
                RtpPacket::parse(packet)
                    .ok()
                    .map(|packet| packet.header.sequence)
            });
        }
        for packet in ready_packets {
            let packet = RtpPacket::parse(&packet)?;
            if self
                .next_sequence
                .is_some_and(|expected| expected != packet.header.sequence)
            {
                losses = losses.saturating_add(1);
                self.depacketizer.reset();
                self.damaged_timestamp = Some(packet.header.timestamp);
            }
            self.next_sequence = Some(packet.header.sequence.wrapping_add(1));
            if self.damaged_timestamp == Some(packet.header.timestamp) {
                continue;
            }
            self.damaged_timestamp = None;
            if let Some(unit) = self.depacketizer.push(&packet)? {
                let Some(position) = self
                    .first_packet_arrivals
                    .iter()
                    .position(|(timestamp, _)| *timestamp == unit.timestamp)
                else {
                    return Err(Error::Invalid(
                        "completed RTP access unit has no first-packet timestamp",
                    ));
                };
                let (_, first_packet_received_at) = self
                    .first_packet_arrivals
                    .remove(position)
                    .expect("timing tracker position was found");
                self.completed.push_back(TimedAccessUnit {
                    unit,
                    first_packet_received_at,
                    completed_at: Instant::now(),
                });
            }
        }
        Ok(losses)
    }
}

struct FeedbackStream {
    remote_ssrc: u32,
    srtcp: SrtcpContext,
}

/// Scan interleaved streams in index order, collect every completed AU, then
/// sort the scheduled items before the four-subframe timestamp barrier.
fn process_completed_frames(
    streams: &mut [InboundVideoStream],
) -> VecDeque<(usize, TimedAccessUnit)> {
    let mut scheduled = Vec::new();
    for (stream_index, stream) in streams.iter_mut().enumerate() {
        while let Some(unit) = stream.completed.pop_front() {
            insert_scheduled_item(&mut scheduled, stream_index, unit);
        }
    }
    scheduled.into()
}

fn insert_scheduled_item(
    scheduled: &mut Vec<(usize, TimedAccessUnit)>,
    stream_index: usize,
    unit: TimedAccessUnit,
) {
    let position = scheduled
        .iter()
        .position(|(_, current)| timestamp_precedes(unit.unit.timestamp, current.unit.timestamp))
        .unwrap_or(scheduled.len());
    // Equal timestamps insert after existing items, preserving the stream
    // scan order above.
    scheduled.insert(position, (stream_index, unit));
}

fn timestamp_precedes(candidate: u32, reference: u32) -> bool {
    (candidate.wrapping_sub(reference) as i32).is_negative()
}

/// Borrowed session credentials for one bidirectional AVC media stream.
pub struct AvcStreamCrypto<'a> {
    pub server_to_viewer_key_blob: &'a [u8],
    pub viewer_to_server_key_blob: &'a [u8],
    pub remote_ssrc: u32,
    pub local_ssrc: u32,
}

impl AvcVideoStreamReceiver {
    pub fn new(
        endpoints: &MediaUdpEndpoints,
        kind: UdpStreamKind,
        crypto: AvcStreamCrypto<'_>,
        codec: MediaStreamCodec,
        payload_type: u8,
    ) -> Result<Self> {
        let session = MediaUdpSession::connect(endpoints, kind)?;
        let mut streams = Vec::with_capacity(AVC_VIDEO_SLICE_COUNT);
        let mut feedback = Vec::with_capacity(AVC_VIDEO_SLICE_COUNT);
        for layer in 0..AVC_VIDEO_SLICE_COUNT as u32 {
            let remote_ssrc = crypto.remote_ssrc.wrapping_add(layer);
            let local_ssrc = crypto.local_ssrc.wrapping_add(layer);
            streams.push(InboundVideoStream::new(
                crypto.server_to_viewer_key_blob,
                remote_ssrc,
                codec,
            )?);
            feedback.push(FeedbackStream {
                remote_ssrc,
                srtcp: SrtcpContext::from_key_blob_with_sender_ssrc(
                    crypto.viewer_to_server_key_blob,
                    local_ssrc,
                )?,
            });
        }
        let mut receiver = Self {
            session,
            streams,
            feedback,
            codec,
            awaiting_sync: true,
            pending_sync_followers: Vec::with_capacity(AVC_VIDEO_SLICE_COUNT - 1),
            expected_payload_type: payload_type,
            base_remote_ssrc: crypto.remote_ssrc,
            buffer: vec![0u8; MAX_RTP_PACKET],
            decrypted_buffer: Vec::with_capacity(MAX_RTP_PACKET),
            frame_batcher: AccessUnitBatcher::default(),
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
        let now = Instant::now();
        if let Some(batch) = self.frame_batcher.take_ready(now) {
            self.frames = self.frames.saturating_add(1);
            return Ok(Some(batch));
        }
        let len = match self.session.recv(&mut self.buffer) {
            Ok(len) => len,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                let batch = self.frame_batcher.take_ready(Instant::now());
                self.frames = self.frames.saturating_add(usize::from(batch.is_some()));
                return Ok(batch);
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
        if stream_index >= self.streams.len() {
            return Ok(None);
        }
        self.packets_received += 1;
        if payload_type != self.expected_payload_type {
            return Err(Error::Invalid("unexpected negotiated RTP payload type"));
        }
        self.decrypted_buffer.clear();
        self.decrypted_buffer
            .extend_from_slice(&self.buffer[..body_len]);
        let stream = &mut self.streams[stream_index];
        stream.srtp.decrypt_authenticated_rtp_packet_in_place(
            &mut self.decrypted_buffer,
            &authentication_tag,
            sequence,
            payload_offset,
        )?;
        self.decrypted_packets += 1;
        let losses = stream.push_decrypted_packet(&self.decrypted_buffer, packet_received_at)?;
        if losses != 0 {
            self.packet_losses = self.packet_losses.saturating_add(losses);
            self.enter_sync_recovery();
            if std::env::var_os("ARD_MEDIA_TRACE").is_some() {
                eprintln!(
                    "RTP loss: stream={stream_index} timestamp={timestamp} sequence={sequence} dropped_access_units={losses} total={}",
                    self.packet_losses,
                );
            }
            self.send_picture_loss_indications()?;
        }
        let completed = process_completed_frames(&mut self.streams);
        let now = Instant::now();
        let mut late_units = 0usize;
        let mut prediction_chain_failure = None;
        'completed: for (slice_index, timed) in completed {
            if timed.unit.decode_order_number.is_none() {
                self.enter_sync_recovery();
                self.send_picture_loss_indications()?;
                return Err(Error::Invalid(
                    "native AVC access unit is missing its DON/DONL decode order",
                ));
            }
            let mut candidates = Vec::with_capacity(AVC_VIDEO_SLICE_COUNT);
            if self.awaiting_sync {
                if !timed.unit.is_sync(self.codec) {
                    self.hold_possible_sync_follower(slice_index, timed);
                    continue;
                }
                // The first IRAP/IDR after startup or PLI is an authoritative
                // new decoding-order origin. UDP may complete a following
                // predictive band first, so retain only same-timestamp DONs
                // immediately following this sync unit.
                let decode_order = timed
                    .unit
                    .decode_order_number
                    .expect("missing DON/DONL was rejected above");
                let timestamp = timed.unit.timestamp;
                self.frame_batcher.begin_prediction_chain(decode_order);
                self.ready_units.clear();
                self.awaiting_sync = false;
                if std::env::var_os("ARD_MEDIA_TRACE").is_some() {
                    eprintln!(
                        "RTP codec sync acquired: stream={slice_index} timestamp={} DON={:?}",
                        timed.unit.timestamp, timed.unit.decode_order_number,
                    );
                }
                candidates.push((slice_index, timed));
                candidates.extend(self.take_sync_followers(slice_index, timestamp, decode_order));
                candidates.sort_by_key(|(_, candidate)| {
                    candidate
                        .unit
                        .decode_order_number
                        .expect("sync followers carry DON/DONL")
                        .wrapping_sub(decode_order)
                });
            } else {
                candidates.push((slice_index, timed));
            }

            for (candidate_index, candidate) in candidates {
                match self.frame_batcher.insert(candidate_index, candidate) {
                    BatchInsertResult::Accepted => {}
                    BatchInsertResult::IgnoredLate => {
                        late_units = late_units.saturating_add(1);
                    }
                    BatchInsertResult::MissingDecodeOrder => {
                        unreachable!("receiver validates DON/DONL before inserting access units")
                    }
                    BatchInsertResult::InvalidPredictionChain => {
                        prediction_chain_failure = Some("duplicate stream in one RTP timestamp");
                        break 'completed;
                    }
                    BatchInsertResult::PredictionChainOverflow => {
                        prediction_chain_failure = Some("pending DON/DONL sequence overflow");
                        break 'completed;
                    }
                }
            }
        }
        if late_units != 0 && std::env::var_os("ARD_MEDIA_TRACE").is_some() {
            eprintln!("RTP ignored completed late/duplicate access units: count={late_units}");
        }
        if let Some(reason) = prediction_chain_failure {
            self.packet_losses = self.packet_losses.saturating_add(1);
            self.enter_sync_recovery();
            if std::env::var_os("ARD_MEDIA_TRACE").is_some() {
                eprintln!(
                    "RTP cross-stream prediction chain reset ({reason}); max_pending_timestamps={MAX_PENDING_FRAME_BATCHES} total_losses={}",
                    self.packet_losses,
                );
            }
            self.send_picture_loss_indications()?;
        }
        let batch = self.frame_batcher.take_ready(now);
        self.frames = self.frames.saturating_add(usize::from(batch.is_some()));
        Ok(batch)
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

    fn send_receiver_reports(&mut self) -> Result<()> {
        for feedback in &mut self.feedback {
            let report = feedback
                .srtcp
                .protect_receiver_report(feedback.remote_ssrc)?;
            self.session.send(&report).map_err(io_error)?;
            self.heartbeats_sent += 1;
        }
        self.last_feedback = Instant::now();
        Ok(())
    }

    pub fn request_keyframe(&mut self) -> Result<()> {
        self.enter_sync_recovery();
        self.send_picture_loss_indications()
    }

    fn hold_possible_sync_follower(&mut self, stream_index: usize, timed: TimedAccessUnit) {
        let timestamp = timed.unit.timestamp;
        if let Some(current_timestamp) = self
            .pending_sync_followers
            .first()
            .map(|(_, candidate)| candidate.unit.timestamp)
            && current_timestamp != timestamp
        {
            if timestamp_precedes(current_timestamp, timestamp) {
                self.pending_sync_followers.clear();
            } else {
                return;
            }
        }
        if self
            .pending_sync_followers
            .iter()
            .any(|(index, candidate)| {
                *index == stream_index
                    || candidate.unit.decode_order_number == timed.unit.decode_order_number
            })
        {
            return;
        }
        if self.pending_sync_followers.len() < AVC_VIDEO_SLICE_COUNT {
            self.pending_sync_followers.push((stream_index, timed));
        }
    }

    fn take_sync_followers(
        &mut self,
        sync_stream_index: usize,
        timestamp: u32,
        decode_order: u16,
    ) -> Vec<(usize, TimedAccessUnit)> {
        std::mem::take(&mut self.pending_sync_followers)
            .into_iter()
            .filter(|(stream_index, candidate)| {
                if *stream_index == sync_stream_index || candidate.unit.timestamp != timestamp {
                    return false;
                }
                candidate
                    .unit
                    .decode_order_number
                    .is_some_and(|candidate_order| {
                        let offset = candidate_order.wrapping_sub(decode_order);
                        (1..AVC_VIDEO_SLICE_COUNT as u16).contains(&offset)
                    })
            })
            .collect()
    }

    fn enter_sync_recovery(&mut self) {
        self.frame_batcher.reset();
        self.ready_units.clear();
        self.awaiting_sync = true;
        self.pending_sync_followers.clear();
        for stream in &mut self.streams {
            stream.depacketizer.reset();
            stream.reorder.reset_pending();
            stream.completed.clear();
            stream.first_packet_arrivals.clear();
            stream.damaged_timestamp = None;
        }
    }

    fn send_picture_loss_indications(&mut self) -> Result<()> {
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
    use std::time::{Duration, Instant};

    use super::{
        AVC_VIDEO_SLICE_COUNT, AccessUnitBatcher, BatchInsertResult, MAX_PENDING_FRAME_BATCHES,
        MediaUdpEndpoints, MediaUdpPortOverrides, MediaUdpSession, TimedAccessUnit, UdpStreamKind,
        insert_scheduled_item, is_rtcp,
    };
    use crate::media_stream::{AccessUnit, ENCODING_AVC_MEDIA_STREAM, MediaStreamMessage1};

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

    fn unit(timestamp: u32, decode_order: u16) -> AccessUnit {
        AccessUnit {
            timestamp,
            decode_order_number: Some(decode_order),
            nal_units: vec![vec![decode_order as u8]],
        }
    }

    fn timed_unit(timestamp: u32, decode_order: u16, at: Instant) -> TimedAccessUnit {
        TimedAccessUnit {
            unit: unit(timestamp, decode_order),
            first_packet_received_at: at,
            completed_at: at,
        }
    }

    #[test]
    fn completed_slices_are_stably_sorted_in_stream_scan_order() {
        let now = Instant::now();
        let mut ready = Vec::new();
        for index in 0..AVC_VIDEO_SLICE_COUNT {
            insert_scheduled_item(&mut ready, index, timed_unit(100, index as u16, now));
        }
        assert_eq!(
            ready
                .iter()
                .map(|(index, timed)| (*index, timed.unit.nal_units[0][0]))
                .collect::<Vec<_>>(),
            vec![(0, 0), (1, 1), (2, 2), (3, 3)]
        );
    }

    #[test]
    fn sparse_updates_are_sorted_without_waiting_for_four_slices() {
        let now = Instant::now();
        let mut ready = Vec::new();
        insert_scheduled_item(&mut ready, 1, timed_unit(200, 1, now));
        insert_scheduled_item(&mut ready, 0, timed_unit(100, 0, now));
        insert_scheduled_item(&mut ready, 2, timed_unit(100, 2, now));
        assert_eq!(
            ready
                .iter()
                .map(|(index, timed)| (*index, timed.unit.timestamp))
                .collect::<Vec<_>>(),
            vec![(0, 100), (2, 100), (1, 200)]
        );
    }

    #[test]
    fn timestamp_sort_is_wrap_aware() {
        let now = Instant::now();
        let mut ready = Vec::new();
        insert_scheduled_item(&mut ready, 2, timed_unit(1, 2, now));
        insert_scheduled_item(&mut ready, 0, timed_unit(u32::MAX - 1, 0, now));
        insert_scheduled_item(&mut ready, 1, timed_unit(u32::MAX, 1, now));
        assert_eq!(
            ready
                .iter()
                .map(|(index, timed)| (*index, timed.unit.timestamp))
                .collect::<Vec<_>>(),
            vec![(0, u32::MAX - 1), (1, u32::MAX), (2, 1)]
        );
    }

    #[test]
    fn complete_four_slice_timestamp_is_released_immediately() {
        let now = Instant::now();
        let mut batcher = AccessUnitBatcher::default();
        batcher.begin_prediction_chain(100);
        for (slice, decode_order) in [(0, 102), (1, 100), (2, 103), (3, 101)] {
            assert_eq!(
                batcher.insert(slice, timed_unit(800, decode_order, now)),
                BatchInsertResult::Accepted
            );
        }
        let batch = batcher.take_ready(now).expect("complete timestamp");
        assert_eq!(batch.timestamp, 800);
        assert_eq!(batch.access_units.len(), AVC_VIDEO_SLICE_COUNT);
        assert_eq!(
            batch
                .access_units
                .iter()
                .map(|(slice, unit)| (*slice, unit.decode_order_number.unwrap()))
                .collect::<Vec<_>>(),
            vec![(1, 100), (3, 101), (0, 102), (2, 103)]
        );
    }

    #[test]
    fn batch_timing_separates_first_packet_from_first_completed_access_unit() {
        let now = Instant::now();
        let mut batcher = AccessUnitBatcher::default();
        batcher.begin_prediction_chain(20);
        for (slice, packet_ms, completed_ms) in [(0, 3, 7), (1, 1, 5), (2, 2, 4), (3, 4, 8)] {
            assert_eq!(
                batcher.insert(
                    slice,
                    TimedAccessUnit {
                        unit: unit(900, 20 + slice as u16),
                        first_packet_received_at: now + Duration::from_millis(packet_ms),
                        completed_at: now + Duration::from_millis(completed_ms),
                    },
                ),
                BatchInsertResult::Accepted
            );
        }
        let batch = batcher
            .take_ready(now + Duration::from_millis(9))
            .expect("complete timestamp");
        assert_eq!(
            batch.first_packet_received_at,
            now + Duration::from_millis(1)
        );
        assert_eq!(
            batch.first_access_unit_completed_at,
            now + Duration::from_millis(4)
        );
        assert_eq!(batch.released_at, now + Duration::from_millis(9));
    }

    #[test]
    fn incomplete_timestamp_is_never_released_by_a_wall_clock_guess() {
        let now = Instant::now();
        let mut batcher = AccessUnitBatcher::default();
        batcher.begin_prediction_chain(40);
        assert_eq!(
            batcher.insert(2, timed_unit(1_600, 42, now)),
            BatchInsertResult::Accepted
        );
        assert!(batcher.take_ready(now).is_none());
        assert!(
            batcher.take_ready(now + Duration::from_secs(1)).is_none(),
            "elapsed time cannot prove a prediction subframe was omitted"
        );
        for (slice, decode_order) in [(0, 40), (1, 41), (3, 43)] {
            assert_eq!(
                batcher.insert(slice, timed_unit(1_600, decode_order, now)),
                BatchInsertResult::Accepted
            );
        }
        let batch = batcher.take_ready(now).expect("complete timestamp");
        assert_eq!(batch.timestamp, 1_600);
        assert_eq!(batch.access_units.len(), AVC_VIDEO_SLICE_COUNT);
    }

    #[test]
    fn a_newer_full_timestamp_cannot_replace_an_incomplete_sync_origin() {
        let now = Instant::now();
        let mut batcher = AccessUnitBatcher::default();
        batcher.begin_prediction_chain(10);
        assert_eq!(
            batcher.insert(0, timed_unit(100, 10, now)),
            BatchInsertResult::Accepted
        );
        for (slice, decode_order) in [(0, 14), (1, 15), (2, 16), (3, 17)] {
            assert_eq!(
                batcher.insert(slice, timed_unit(200, decode_order, now)),
                BatchInsertResult::Accepted
            );
        }
        assert!(
            batcher.take_ready(now).is_none(),
            "decoder must not skip the sync timestamp's missing DONs"
        );
    }

    #[test]
    fn sparse_timestamp_releases_only_when_next_don_proves_its_boundary() {
        let now = Instant::now();
        let mut batcher = AccessUnitBatcher::default();
        batcher.begin_prediction_chain(0);
        for slice in 0..AVC_VIDEO_SLICE_COUNT {
            assert_eq!(
                batcher.insert(slice, timed_unit(100, slice as u16, now)),
                BatchInsertResult::Accepted
            );
        }
        batcher.take_ready(now).expect("initial sync batch");

        assert_eq!(
            batcher.insert(3, timed_unit(200, 5, now)),
            BatchInsertResult::Accepted
        );
        assert_eq!(
            batcher.insert(0, timed_unit(200, 4, now)),
            BatchInsertResult::Accepted
        );
        assert!(batcher.take_ready(now + Duration::from_secs(1)).is_none());

        assert_eq!(
            batcher.insert(1, timed_unit(300, 6, now)),
            BatchInsertResult::Accepted
        );
        let sparse = batcher
            .take_ready(now)
            .expect("DON boundary proves sparse batch");
        assert_eq!(sparse.timestamp, 200);
        assert_eq!(
            sparse
                .access_units
                .iter()
                .map(|(slice, unit)| (*slice, unit.decode_order_number.unwrap()))
                .collect::<Vec<_>>(),
            vec![(0, 4), (3, 5)]
        );
    }

    #[test]
    fn a_decode_order_gap_blocks_newer_timestamps_until_the_missing_unit_arrives() {
        let now = Instant::now();
        let mut batcher = AccessUnitBatcher::default();
        batcher.begin_prediction_chain(0);
        for slice in 0..AVC_VIDEO_SLICE_COUNT {
            batcher.insert(slice, timed_unit(100, slice as u16, now));
        }
        batcher.take_ready(now).expect("initial sync batch");

        assert_eq!(
            batcher.insert(3, timed_unit(200, 5, now)),
            BatchInsertResult::Accepted
        );
        assert_eq!(
            batcher.insert(0, timed_unit(300, 6, now)),
            BatchInsertResult::Accepted
        );
        assert!(batcher.take_ready(now).is_none(), "DON 4 is still missing");
        assert_eq!(
            batcher.insert(1, timed_unit(200, 4, now)),
            BatchInsertResult::Accepted
        );
        let recovered = batcher.take_ready(now).expect("contiguous DON chain");
        assert_eq!(recovered.timestamp, 200);
        assert_eq!(
            recovered
                .access_units
                .iter()
                .map(|(_, unit)| unit.decode_order_number.unwrap())
                .collect::<Vec<_>>(),
            vec![4, 5]
        );
    }

    #[test]
    fn decode_order_wrap_is_contiguous_and_late_units_are_rejected() {
        let now = Instant::now();
        let mut batcher = AccessUnitBatcher::default();
        batcher.begin_prediction_chain(u16::MAX - 1);
        for (slice, decode_order) in [(0, u16::MAX), (1, 1), (2, u16::MAX - 1), (3, 0)] {
            assert_eq!(
                batcher.insert(slice, timed_unit(u32::MAX, decode_order, now)),
                BatchInsertResult::Accepted
            );
        }
        let first = batcher.take_ready(now).expect("wrapped initial batch");
        assert_eq!(
            first
                .access_units
                .iter()
                .map(|(_, unit)| unit.decode_order_number.unwrap())
                .collect::<Vec<_>>(),
            vec![u16::MAX - 1, u16::MAX, 0, 1]
        );
        assert_eq!(
            batcher.insert(3, timed_unit(1, u16::MAX, now)),
            BatchInsertResult::IgnoredLate
        );
        assert_eq!(
            batcher.insert(0, timed_unit(1, 2, now)),
            BatchInsertResult::Accepted
        );
        assert_eq!(
            batcher.insert(1, timed_unit(2, 3, now)),
            BatchInsertResult::Accepted
        );
        let sparse = batcher.take_ready(now).expect("post-wrap sparse batch");
        assert_eq!(sparse.access_units[0].1.decode_order_number, Some(2));
    }

    #[test]
    fn missing_decode_order_and_duplicate_stream_are_explicit_errors() {
        let now = Instant::now();
        let mut batcher = AccessUnitBatcher::default();
        let mut missing = timed_unit(100, 0, now);
        missing.unit.decode_order_number = None;
        assert_eq!(
            batcher.insert(0, missing),
            BatchInsertResult::MissingDecodeOrder
        );
        assert_eq!(
            batcher.insert(0, timed_unit(100, 0, now)),
            BatchInsertResult::Accepted
        );
        assert_eq!(
            batcher.insert(0, timed_unit(100, 1, now)),
            BatchInsertResult::InvalidPredictionChain
        );
    }

    #[test]
    fn incomplete_prediction_chain_is_bounded_and_reset() {
        let now = Instant::now();
        let mut batcher = AccessUnitBatcher::default();
        for frame in 0..MAX_PENDING_FRAME_BATCHES {
            assert_eq!(
                batcher.insert(0, timed_unit(100 + frame as u32, 10 + frame as u16, now),),
                BatchInsertResult::Accepted
            );
        }
        assert_eq!(
            batcher.insert(0, timed_unit(200, 30, now)),
            BatchInsertResult::PredictionChainOverflow
        );
        assert!(batcher.pending.is_empty());
        assert_eq!(batcher.last_released_decode_order, None);
    }
}
