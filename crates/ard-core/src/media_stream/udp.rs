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
const DEFAULT_FRAME_HOLDBACK: Duration = Duration::from_millis(17);
const MIN_FRAME_HOLDBACK: Duration = Duration::from_millis(6);
const MAX_FRAME_HOLDBACK: Duration = Duration::from_millis(34);
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

/// Remote UDP endpoints derived from the server's base port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaUdpEndpoints {
    pub host: IpAddr,
    pub video1_port: u16,
    pub video2_port: Option<u16>,
    pub audio_port: Option<u16>,
}

impl MediaUdpEndpoints {
    /// Build endpoints from a confirmed `MediaStreamMessage1`.
    pub fn from_message1(host: IpAddr, message: &MediaStreamMessage1) -> Self {
        Self {
            host,
            video1_port: message.video1_port,
            video2_port: message.video2_port,
            audio_port: message.audio_port,
        }
    }

    pub fn port_for(&self, kind: UdpStreamKind) -> Option<u16> {
        match kind {
            UdpStreamKind::Video1 => Some(self.video1_port),
            UdpStreamKind::Video2 => self.video2_port,
            UdpStreamKind::Audio => self.audio_port,
        }
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
    pub fn connect(endpoints: &MediaUdpEndpoints, kind: UdpStreamKind) -> Result<Self> {
        let port = endpoints
            .port_for(kind)
            .ok_or(Error::Invalid("endpoint not offered for stream kind"))?;
        let remote = SocketAddr::new(endpoints.host, port);
        let bind_addr = match endpoints.host {
            IpAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port),
            IpAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port),
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

/// Every changed horizontal band sharing one RTP sampling instant. RFC 3550
/// defines the RTP timestamp as the sampling instant; Apple's four adjacent
/// SSRCs reuse that timestamp for the bands belonging to one desktop frame.
#[derive(Debug)]
pub struct AvcFrameBatch {
    pub timestamp: u32,
    pub access_units: Vec<(usize, AccessUnit)>,
}

struct PendingFrameBatch {
    timestamp: u32,
    first_seen: Instant,
    access_units: Vec<(usize, AccessUnit)>,
}

#[derive(Default)]
struct AccessUnitBatcher {
    pending: Vec<PendingFrameBatch>,
    last_released_timestamp: Option<u32>,
    estimated_frame_ticks: Option<u32>,
}

impl AccessUnitBatcher {
    fn insert(&mut self, stream_index: usize, unit: AccessUnit, now: Instant) -> bool {
        if self.last_released_timestamp.is_some_and(|released| {
            unit.timestamp == released || !timestamp_precedes(released, unit.timestamp)
        }) {
            return false;
        }
        if let Some(batch) = self
            .pending
            .iter_mut()
            .find(|batch| batch.timestamp == unit.timestamp)
        {
            if let Some(existing) = batch
                .access_units
                .iter_mut()
                .find(|(index, _)| *index == stream_index)
            {
                *existing = (stream_index, unit);
            } else {
                batch.access_units.push((stream_index, unit));
                batch.access_units.sort_by_key(|(index, _)| *index);
            }
            return true;
        }

        if let Some(latest) = self.pending.last()
            && timestamp_precedes(latest.timestamp, unit.timestamp)
        {
            let delta = unit.timestamp.wrapping_sub(latest.timestamp);
            if delta != 0 && delta < 90_000 {
                self.estimated_frame_ticks = Some(match self.estimated_frame_ticks {
                    Some(previous) => previous.saturating_mul(7).saturating_add(delta) / 8,
                    None => delta,
                });
            }
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
                first_seen: now,
                access_units: vec![(stream_index, unit)],
            },
        );
        true
    }

    fn take_ready(&mut self, now: Instant) -> Option<AvcFrameBatch> {
        let ready = self.pending.first().is_some_and(|batch| {
            batch.access_units.len() == AVC_VIDEO_SLICE_COUNT
                || now.duration_since(batch.first_seen) >= self.holdback()
                || self.pending.len() >= 3
        });
        if !ready {
            return None;
        }
        let batch = self.pending.remove(0);
        self.last_released_timestamp = Some(batch.timestamp);
        Some(AvcFrameBatch {
            timestamp: batch.timestamp,
            access_units: batch.access_units,
        })
    }

    fn holdback(&self) -> Duration {
        let Some(ticks) = self.estimated_frame_ticks else {
            return DEFAULT_FRAME_HOLDBACK;
        };
        let micros = u64::from(ticks)
            .saturating_mul(1_000_000)
            .checked_div(90_000)
            .unwrap_or_default();
        Duration::from_micros(micros)
            .saturating_add(Duration::from_millis(2))
            .clamp(MIN_FRAME_HOLDBACK, MAX_FRAME_HOLDBACK)
    }
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
    completed: VecDeque<AccessUnit>,
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
            next_sequence: None,
            damaged_timestamp: None,
        })
    }

    fn push_decrypted_packet(&mut self, packet: &[u8]) -> Result<usize> {
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
                self.completed.push_back(unit);
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
/// sort the scheduled items before decode. This is the native receiver's
/// process/schedule/insert sequence and intentionally has no four-slice
/// timestamp barrier: unchanged desktop bands may be absent.
fn process_completed_frames(streams: &mut [InboundVideoStream]) -> VecDeque<(usize, AccessUnit)> {
    let mut scheduled = Vec::new();
    for (stream_index, stream) in streams.iter_mut().enumerate() {
        while let Some(unit) = stream.completed.pop_front() {
            insert_scheduled_item(&mut scheduled, stream_index, unit);
        }
    }
    scheduled.into()
}

fn insert_scheduled_item(
    scheduled: &mut Vec<(usize, AccessUnit)>,
    stream_index: usize,
    unit: AccessUnit,
) {
    let position = scheduled
        .iter()
        .position(|(_, current)| timestamp_precedes(unit.timestamp, current.timestamp))
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

    /// Receive one complete desktop sampling instant. A full four-band frame
    /// is released immediately; sparse updates are held for one measured RTP
    /// frame interval so a delayed band cannot be decoded after a newer frame.
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
        let (sequence, payload_offset, payload_type, ssrc) = {
            let encrypted_packet = RtpPacket::parse_encrypted(&self.buffer[..body_len])?;
            (
                encrypted_packet.header.sequence,
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
        let losses = stream.push_decrypted_packet(&self.decrypted_buffer)?;
        if losses != 0 {
            self.packet_losses = self.packet_losses.saturating_add(losses);
            self.send_picture_loss_indications()?;
        }
        let completed = process_completed_frames(&mut self.streams);
        let now = Instant::now();
        let mut late_units = 0usize;
        for (slice_index, unit) in completed {
            if !self.frame_batcher.insert(slice_index, unit, now) {
                late_units = late_units.saturating_add(1);
            }
        }
        if late_units != 0 {
            self.packet_losses = self.packet_losses.saturating_add(late_units);
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
        self.send_picture_loss_indications()
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
    use std::time::{Duration, Instant};

    use super::{AVC_VIDEO_SLICE_COUNT, AccessUnitBatcher, insert_scheduled_item, is_rtcp};
    use crate::media_stream::AccessUnit;

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

    fn unit(timestamp: u32, value: u8) -> AccessUnit {
        AccessUnit {
            timestamp,
            nal_units: vec![vec![value]],
        }
    }

    #[test]
    fn completed_slices_are_stably_sorted_in_stream_scan_order() {
        let mut ready = Vec::new();
        for index in 0..AVC_VIDEO_SLICE_COUNT {
            insert_scheduled_item(&mut ready, index, unit(100, index as u8));
        }
        assert_eq!(
            ready
                .iter()
                .map(|(index, unit)| (*index, unit.nal_units[0][0]))
                .collect::<Vec<_>>(),
            vec![(0, 0), (1, 1), (2, 2), (3, 3)]
        );
    }

    #[test]
    fn sparse_updates_are_sorted_without_waiting_for_four_slices() {
        let mut ready = Vec::new();
        insert_scheduled_item(&mut ready, 1, unit(200, 1));
        insert_scheduled_item(&mut ready, 0, unit(100, 0));
        insert_scheduled_item(&mut ready, 2, unit(100, 2));
        assert_eq!(
            ready
                .iter()
                .map(|(index, unit)| (*index, unit.timestamp))
                .collect::<Vec<_>>(),
            vec![(0, 100), (2, 100), (1, 200)]
        );
    }

    #[test]
    fn timestamp_sort_is_wrap_aware() {
        let mut ready = Vec::new();
        insert_scheduled_item(&mut ready, 2, unit(1, 2));
        insert_scheduled_item(&mut ready, 0, unit(u32::MAX - 1, 0));
        insert_scheduled_item(&mut ready, 1, unit(u32::MAX, 1));
        assert_eq!(
            ready
                .iter()
                .map(|(index, unit)| (*index, unit.timestamp))
                .collect::<Vec<_>>(),
            vec![(0, u32::MAX - 1), (1, u32::MAX), (2, 1)]
        );
    }

    #[test]
    fn complete_four_slice_timestamp_is_released_immediately() {
        let now = Instant::now();
        let mut batcher = AccessUnitBatcher::default();
        for slice in 0..AVC_VIDEO_SLICE_COUNT {
            assert!(batcher.insert(slice, unit(800, slice as u8), now));
        }
        let batch = batcher.take_ready(now).expect("complete timestamp");
        assert_eq!(batch.timestamp, 800);
        assert_eq!(batch.access_units.len(), AVC_VIDEO_SLICE_COUNT);
        assert_eq!(
            batch
                .access_units
                .iter()
                .map(|(slice, _)| *slice)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
    }

    #[test]
    fn sparse_timestamp_waits_one_frame_interval_without_waiting_for_all_slices() {
        let now = Instant::now();
        let mut batcher = AccessUnitBatcher::default();
        assert!(batcher.insert(2, unit(1_600, 2), now));
        assert!(batcher.take_ready(now).is_none());
        let batch = batcher
            .take_ready(now + Duration::from_millis(18))
            .expect("sparse timestamp after bounded holdback");
        assert_eq!(batch.timestamp, 1_600);
        assert_eq!(batch.access_units.len(), 1);
        assert_eq!(batch.access_units[0].0, 2);
    }

    #[test]
    fn timestamp_batches_preserve_wrap_order_and_reject_late_old_units() {
        let now = Instant::now();
        let mut batcher = AccessUnitBatcher::default();
        assert!(batcher.insert(0, unit(u32::MAX - 1, 0), now));
        assert!(batcher.insert(1, unit(u32::MAX, 1), now));
        assert!(batcher.insert(2, unit(1, 2), now));
        let first = batcher.take_ready(now).expect("oldest wrap batch");
        assert_eq!(first.timestamp, u32::MAX - 1);
        assert!(!batcher.insert(3, unit(u32::MAX - 1, 3), now));
    }
}
