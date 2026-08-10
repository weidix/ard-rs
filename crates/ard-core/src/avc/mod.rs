//! Apple AVC media stream mode (encoding 1010, `kSSVideoEncoding_AVCMediaStream`).
//!
//! This is the third Screen Sharing video path, distinct from the VNC/RFB
//! rectangle encodings and from MVS (`1011`). It negotiates a real-time
//! H.264/HEVC media stream over UDP instead of drawing rectangles over the
//! TCP RFB session:
//!
//! 1. The viewer sends an RFB client message (`0x1c`) carrying a session UUID,
//!    three stream offers (audio, video 1, video 2) and six 46-byte SRTP keys.
//! 2. The server answers with RFB server message `0x23` (RFBMediaStreamMessage1)
//!    that carries the AVC encoding marker (`1010`) and the UDP base port, and
//!    later with the negotiator answer (RFBMediaStreamMessage2).
//! 3. Encoded H.264/HEVC RTP packets arrive on consecutive UDP ports and are
//!    decrypted (SRTP AES-128-CM) before platform decoding.
//!
//! The module is deliberately isolated from the rectangle pipeline in
//! [`crate::client`], [`crate::decoder`] and [`crate::mvs`]: it never writes
//! into the framebuffer and never touches the VNC encodings.
//!
//! Wire findings were derived from macOS 26.6 (build 25G72) Screen Sharing
//! 6.1 (760.4); see `docs/SCREENSHARING_RE.md` for the reverse-engineering
//! notes.

pub mod negotiation;
pub mod rtp;
pub mod srtp;
pub mod udp;
pub mod wire;

pub use negotiation::{
    MediaStreamCodec, MediaStreamOffer, build_media_stream_offer, build_remote_endpoint_info,
    parse_negotiation_payload,
};
pub use rtp::{AccessUnit, H264Depacketizer, HevcDepacketizer, RtpHeader, RtpPacket};
pub use udp::{MediaUdpEndpoints, MediaUdpSession, UdpStreamKind};
pub use wire::{
    MediaStreamAnswer, MediaStreamConfiguration, MediaStreamError, MediaStreamFlags,
    MediaStreamKeyMaterial, MediaStreamMessage1, MediaStreamServerReply,
};

/// Wire value of `kSSVideoEncoding_AVCMediaStream`.
pub const ENCODING_AVC_MEDIA_STREAM: i32 = 1010;

/// RFB client message type for the media-stream configuration offer.
pub const CLIENT_MEDIA_STREAM_MESSAGE_TYPE: u8 = 0x1c;

/// RFB server message type for RFBMediaStreamMessage1 (Encoding).
pub const SERVER_MEDIA_STREAM_MESSAGE_TYPE: u8 = 0x23;

/// Negotiation message version confirmed in macOS 26.6 Screen Sharing.
pub const MEDIA_STREAM_MESSAGE_VERSION: u16 = 0x0300;

/// Length of each SRTP key blob exchanged in the offer (random 46 bytes).
pub const MEDIA_STREAM_KEY_LEN: usize = 46;

/// Maximum accepted size for a negotiation message (offers are small plists).
pub const MAX_MEDIA_STREAM_MESSAGE: usize = 64 * 1024;

/// Maximum accepted size for a single RTP datagram.
pub const MAX_RTP_PACKET: usize = 64 * 1024;

/// Maximum number of NAL units in one assembled access unit.
pub const MAX_NAL_UNITS_PER_ACCESS_UNIT: usize = 4096;
