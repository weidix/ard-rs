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
use super::rtp::{AccessUnit, H264Depacketizer, HevcDepacketizer, RtpPacket, RtpReorderBuffer};
use super::srtp::SrtpContext;
use super::wire::MediaStreamMessage1;

const RTCP_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(250);
/// Native AVC's 22-byte receiver report/keep-alive. The server does not keep
/// the video RTP stream flowing until it receives this exact packet shape.
const AVC_RTCP_HEARTBEAT: [u8; 22] = [
    0x80, 0xc0, 0x00, 0x01, 0xb5, 0xff, 0x00, 0x3e, 0x80, 0x00, 0x00, 0x01, 0xd6, 0x33, 0xcd, 0x7a,
    0xac, 0x7e, 0x44, 0x4b, 0xd3, 0xd0,
];

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
    srtp: SrtpContext,
    depacketizer: VideoDepacketizer,
    expected_payload_type: u8,
    expected_ssrc: u32,
    buffer: Vec<u8>,
    decrypted_buffer: Vec<u8>,
    reorder: RtpReorderBuffer,
    ready_frames: VecDeque<AccessUnit>,
    packets_received: usize,
    decrypted_packets: usize,
    heartbeats_sent: usize,
    frames: usize,
    last_heartbeat: Instant,
}

enum VideoDepacketizer {
    H264(H264Depacketizer),
    Hevc(HevcDepacketizer),
}

impl AvcVideoStreamReceiver {
    pub fn new(
        endpoints: &MediaUdpEndpoints,
        kind: UdpStreamKind,
        key_blob: &[u8],
        codec: super::negotiation::MediaStreamCodec,
        payload_type: u8,
        remote_ssrc: u32,
    ) -> Result<Self> {
        let session = MediaUdpSession::connect(endpoints, kind)?;
        let srtp = SrtpContext::from_key_blob_with_derived_ssrc(key_blob, remote_ssrc)?;
        let depacketizer = match codec {
            super::negotiation::MediaStreamCodec::H264 => {
                VideoDepacketizer::H264(H264Depacketizer::new())
            }
            super::negotiation::MediaStreamCodec::Hevc => {
                VideoDepacketizer::Hevc(HevcDepacketizer::new_with_donl())
            }
        };
        let mut receiver = Self {
            session,
            srtp,
            depacketizer,
            expected_payload_type: payload_type,
            expected_ssrc: remote_ssrc,
            buffer: vec![0u8; MAX_RTP_PACKET],
            decrypted_buffer: Vec::with_capacity(MAX_RTP_PACKET),
            reorder: RtpReorderBuffer::with_codec(codec),
            ready_frames: VecDeque::new(),
            packets_received: 0,
            decrypted_packets: 0,
            heartbeats_sent: 0,
            frames: 0,
            last_heartbeat: Instant::now(),
        };
        receiver.send_heartbeat()?;
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

    /// Block on one datagram; returns the decoded access unit when a frame
    /// completes, or `Ok(None)` for mid-frame packets.
    pub fn receive(&mut self) -> Result<Option<AccessUnit>> {
        self.send_heartbeat_if_due()?;
        if let Some(frame) = self.ready_frames.pop_front() {
            return Ok(Some(frame));
        }
        let len = match self.session.recv(&mut self.buffer) {
            Ok(len) => len,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                self.send_heartbeat()?;
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
        // The host emits adjacent SSRCs for auxiliary/sub-stream layers on
        // the same UDP endpoint. The negotiated answer identifies the base
        // stream; mixing their independent sequence spaces breaks replay and
        // RTP reordering state.
        if ssrc != self.expected_ssrc {
            return Ok(None);
        }
        self.packets_received += 1;
        if payload_type != self.expected_payload_type {
            return Err(Error::Invalid("unexpected negotiated RTP payload type"));
        }
        self.decrypted_buffer.clear();
        self.decrypted_buffer
            .extend_from_slice(&self.buffer[..body_len]);
        self.srtp.decrypt_authenticated_rtp_packet_in_place(
            &mut self.decrypted_buffer,
            &authentication_tag,
            sequence,
            payload_offset,
        )?;
        self.decrypted_packets += 1;
        let ready_packets = self.reorder.push(&self.decrypted_buffer)?;
        let mut frame = None;
        for packet in ready_packets {
            let packet = RtpPacket::parse(&packet)?;
            let completed = match &mut self.depacketizer {
                VideoDepacketizer::H264(depacketizer) => depacketizer.push(&packet)?,
                VideoDepacketizer::Hevc(depacketizer) => depacketizer.push(&packet)?,
            };
            if let Some(completed) = completed {
                self.frames += 1;
                if frame.is_none() {
                    frame = Some(completed);
                } else {
                    self.ready_frames.push_back(completed);
                }
            }
        }
        Ok(frame)
    }

    fn send_heartbeat_if_due(&mut self) -> Result<()> {
        if self.last_heartbeat.elapsed() >= RTCP_HEARTBEAT_INTERVAL {
            self.send_heartbeat()?;
        }
        Ok(())
    }

    fn send_heartbeat(&mut self) -> Result<()> {
        self.session.send(&AVC_RTCP_HEARTBEAT).map_err(io_error)?;
        self.heartbeats_sent += 1;
        self.last_heartbeat = Instant::now();
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
    use super::is_rtcp;

    #[test]
    fn distinguishes_rtcp_from_rtp() {
        assert!(is_rtcp(&[0x80, 200, 0, 1]));
        assert!(is_rtcp(&[0x80, 206, 0, 1]));
        assert!(!is_rtcp(&[0x80, 96, 0, 1]));
        assert!(!is_rtcp(&[0x40, 200, 0, 1]));
    }

    #[test]
    fn native_heartbeat_has_the_expected_wire_length() {
        assert_eq!(super::AVC_RTCP_HEARTBEAT.len(), 22);
        assert!(is_rtcp(&super::AVC_RTCP_HEARTBEAT));
    }
}
