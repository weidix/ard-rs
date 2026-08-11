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
            .set_read_timeout(Some(Duration::from_millis(250)))
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
    ready_frames: VecDeque<(usize, AccessUnit)>,
    packets_received: usize,
    decrypted_packets: usize,
    heartbeats_sent: usize,
    frames: usize,
    last_feedback: Instant,
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
}

struct InboundVideoStream {
    srtp: SrtpContext,
    reorder: RtpReorderBuffer,
    depacketizer: VideoDepacketizer,
    completed: VecDeque<AccessUnit>,
}

impl InboundVideoStream {
    fn new(key_blob: &[u8], ssrc: u32, codec: MediaStreamCodec) -> Result<Self> {
        Ok(Self {
            srtp: SrtpContext::from_key_blob_with_derived_ssrc(key_blob, ssrc)?,
            reorder: RtpReorderBuffer::with_codec(codec),
            depacketizer: VideoDepacketizer::new(codec),
            completed: VecDeque::new(),
        })
    }

    fn push_decrypted_packet(&mut self, packet: &[u8]) -> Result<()> {
        let ready_packets = self.reorder.push(packet)?;
        for packet in ready_packets {
            let packet = RtpPacket::parse(&packet)?;
            if let Some(unit) = self.depacketizer.push(&packet)? {
                self.completed.push_back(unit);
            }
        }
        Ok(())
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
            ready_frames: VecDeque::new(),
            packets_received: 0,
            decrypted_packets: 0,
            heartbeats_sent: 0,
            frames: 0,
            last_feedback: Instant::now(),
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

    /// Block on one datagram; returns the slice index and completed encoded
    /// access unit, or `Ok(None)` for a mid-frame packet.
    pub fn receive(&mut self) -> Result<Option<(usize, AccessUnit)>> {
        self.send_feedback_if_due()?;
        if let Some(frame) = self.ready_frames.pop_front() {
            return Ok(Some(frame));
        }
        let len = match self.session.recv(&mut self.buffer) {
            Ok(len) => len,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Ok(None);
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
        stream.push_decrypted_packet(&self.decrypted_buffer)?;
        let completed = process_completed_frames(&mut self.streams);
        self.frames += completed.len();
        self.ready_frames.extend(completed);
        Ok(self.ready_frames.pop_front())
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
    use super::{AVC_VIDEO_SLICE_COUNT, insert_scheduled_item, is_rtcp};
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
}
