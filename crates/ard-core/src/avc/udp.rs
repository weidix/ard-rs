//! UDP transport for the AVC media stream.
//!
//! The server hands out a base UDP port in `MediaStreamMessage1`; the client
//! binds the same local port per stream and connects to the consecutive remote
//! ports: video 1 on the base port, video 2 on base+1 and audio on base+2.
//! Packets are standard RTP (RFC 3550) with SRTP AES-128-CM payload
//! encryption.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::time::Duration;

use crate::{Error, Result};

use super::MAX_RTP_PACKET;
use super::rtp::{AccessUnit, H264Depacketizer, HevcDepacketizer, RtpPacket};
use super::srtp::SrtpContext;
use super::wire::MediaStreamMessage1;

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
    buffer: Vec<u8>,
    frames: usize,
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
    ) -> Result<Self> {
        let session = MediaUdpSession::connect(endpoints, kind)?;
        let srtp = SrtpContext::from_key_blob(key_blob)?;
        let depacketizer = match codec {
            super::negotiation::MediaStreamCodec::H264 => {
                VideoDepacketizer::H264(H264Depacketizer::new())
            }
            super::negotiation::MediaStreamCodec::Hevc => {
                VideoDepacketizer::Hevc(HevcDepacketizer::new())
            }
        };
        Ok(Self {
            session,
            srtp,
            depacketizer,
            buffer: vec![0u8; MAX_RTP_PACKET],
            frames: 0,
        })
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Block on one datagram; returns the decoded access unit when a frame
    /// completes, or `Ok(None)` for mid-frame packets.
    pub fn receive(&mut self) -> Result<Option<AccessUnit>> {
        let len = self.session.recv(&mut self.buffer).map_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock
                || error.kind() == std::io::ErrorKind::TimedOut
            {
                Error::Invalid("RTP receive timed out")
            } else {
                Error::Invalid("RTP receive failed")
            }
        })?;
        if len > MAX_RTP_PACKET {
            return Err(Error::LimitExceeded("RTP datagram"));
        }
        if is_rtcp(&self.buffer[..len]) {
            return Ok(None);
        }
        let packet = RtpPacket::parse(&self.buffer[..len])?;
        let plain = self.srtp.decrypt_rtp_payload(
            packet.header.ssrc,
            packet.header.sequence,
            packet.payload,
        )?;
        // Re-parse the packet with the decrypted payload substituted.
        let decrypted_packet = RtpPacket {
            header: packet.header,
            payload: &plain,
            wire_len: plain.len() + 12,
        };
        let frame = match &mut self.depacketizer {
            VideoDepacketizer::H264(depacketizer) => depacketizer.push(&decrypted_packet)?,
            VideoDepacketizer::Hevc(depacketizer) => depacketizer.push(&decrypted_packet)?,
        };
        if frame.is_some() {
            self.frames += 1;
        }
        Ok(frame)
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
}
