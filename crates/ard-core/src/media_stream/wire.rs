//! Wire parsing and serialization for Apple media stream negotiation.
//!
//! The layout below was recovered from macOS 26.6 (build 25G72) binaries:
//!
//! * Viewer to server offer: RFB client message `0x1c`, version `0x0300`
//!   (confirmed against `_RFBMediaStreamServerConfiguration` in
//!   ScreenSharing.framework).
//! * Server to viewer Message1: RFB server message `0x23` carrying the
//!   encoding marker `1010` and consecutive UDP ports (audio at the base
//!   port, video 1 at base+1, and video 2 at base+2; confirmed against
//!   `EncodeRFBMediaStreamMessage1` in screensharingd).
//! * Server to viewer Message2 (answer) and error message builders were
//!   confirmed the same way.
//!
//! All parsers are bounded and reject messages whose length fields disagree
//! with the available bytes.

use std::fmt;

use crate::{Error, Result};

use super::{
    CLIENT_MEDIA_STREAM_MESSAGE_TYPE, MAX_MEDIA_STREAM_MESSAGE, MEDIA_STREAM_KEY_LEN,
    SERVER_MEDIA_STREAM_MESSAGE_TYPE,
};

const HEADER_LEN: usize = 20;
const SESSION_ID_OFFSET: usize = 0x14;
const SESSION_ID_LEN: usize = 16;

/// Negotiation flags carried at message offset `0x06`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MediaStreamFlags(u32);

impl MediaStreamFlags {
    pub const VIDEO1_60FPS: u32 = 0x0000_0001;
    pub const VIDEO2_60FPS: u32 = 0x0000_0002;
    pub const SEND_CURSOR: u32 = 0x0000_0004;
    pub const VIEWER_APP: u32 = 0x0000_0008;

    pub fn new(raw: u32) -> Self {
        Self(raw)
    }

    pub fn raw(self) -> u32 {
        self.0
    }

    pub fn video1_60fps(self) -> bool {
        self.0 & Self::VIDEO1_60FPS != 0
    }

    pub fn video2_60fps(self) -> bool {
        self.0 & Self::VIDEO2_60FPS != 0
    }

    pub fn send_cursor(self) -> bool {
        self.0 & Self::SEND_CURSOR != 0
    }

    pub fn viewer_app(self) -> bool {
        self.0 & Self::VIEWER_APP != 0
    }
}

/// Six SRTP key blobs exchanged during negotiation.
///
/// Every key is 46 random bytes (confirmed: `AuthGetRandomBytes(0x2e)`);
/// the first 32 bytes are the native master key and the final 14 bytes are
/// the native master salt. The native AVC path derives the suite-5 session
/// material from this whole blob before applying AES-CTR.
#[derive(Clone, PartialEq, Eq)]
pub struct MediaStreamKeyMaterial {
    pub audio_viewer_to_server: [u8; MEDIA_STREAM_KEY_LEN],
    pub audio_server_to_viewer: [u8; MEDIA_STREAM_KEY_LEN],
    pub video1_viewer_to_server: [u8; MEDIA_STREAM_KEY_LEN],
    pub video1_server_to_viewer: [u8; MEDIA_STREAM_KEY_LEN],
    pub video2_viewer_to_server: Option<[u8; MEDIA_STREAM_KEY_LEN]>,
    pub video2_server_to_viewer: Option<[u8; MEDIA_STREAM_KEY_LEN]>,
}

impl fmt::Debug for MediaStreamKeyMaterial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MediaStreamKeyMaterial")
            .field(
                "audio_viewer_to_server_len",
                &self.audio_viewer_to_server.len(),
            )
            .field(
                "audio_server_to_viewer_len",
                &self.audio_server_to_viewer.len(),
            )
            .field(
                "video1_viewer_to_server_len",
                &self.video1_viewer_to_server.len(),
            )
            .field(
                "video1_server_to_viewer_len",
                &self.video1_server_to_viewer.len(),
            )
            .field(
                "video2_viewer_to_server_present",
                &self.video2_viewer_to_server.is_some(),
            )
            .field(
                "video2_server_to_viewer_present",
                &self.video2_server_to_viewer.is_some(),
            )
            .finish()
    }
}

impl MediaStreamKeyMaterial {
    /// Create key material from slices, requiring each provided slice to be
    /// exactly [`MEDIA_STREAM_KEY_LEN`] bytes.
    pub fn new(
        audio_viewer_to_server: &[u8],
        audio_server_to_viewer: &[u8],
        video1_viewer_to_server: &[u8],
        video1_server_to_viewer: &[u8],
        video2_viewer_to_server: Option<&[u8]>,
        video2_server_to_viewer: Option<&[u8]>,
    ) -> Result<Self> {
        fn key(slice: &[u8]) -> Result<[u8; MEDIA_STREAM_KEY_LEN]> {
            slice
                .try_into()
                .map_err(|_| Error::Invalid("media stream key must be 46 bytes"))
        }
        let (v2s, s2v) = match (video2_viewer_to_server, video2_server_to_viewer) {
            (Some(a), Some(b)) => (Some(key(a)?), Some(key(b)?)),
            (None, None) => (None, None),
            _ => return Err(Error::Invalid("video2 keys must be provided together")),
        };
        Ok(Self {
            audio_viewer_to_server: key(audio_viewer_to_server)?,
            audio_server_to_viewer: key(audio_server_to_viewer)?,
            video1_viewer_to_server: key(video1_viewer_to_server)?,
            video1_server_to_viewer: key(video1_server_to_viewer)?,
            video2_viewer_to_server: v2s,
            video2_server_to_viewer: s2v,
        })
    }
}

impl Drop for MediaStreamKeyMaterial {
    fn drop(&mut self) {
        self.audio_viewer_to_server.fill(0);
        self.audio_server_to_viewer.fill(0);
        self.video1_viewer_to_server.fill(0);
        self.video1_server_to_viewer.fill(0);
        if let Some(key) = &mut self.video2_viewer_to_server {
            key.fill(0);
        }
        if let Some(key) = &mut self.video2_server_to_viewer {
            key.fill(0);
        }
    }
}

/// The viewer's media-stream configuration offer (RFB client message `0x1c`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaStreamConfiguration {
    pub message_version: u16,
    pub flags: MediaStreamFlags,
    pub session_id: [u8; SESSION_ID_LEN],
    pub audio_offer: Vec<u8>,
    pub video1_offer: Vec<u8>,
    pub video2_offer: Option<Vec<u8>>,
    pub keys: MediaStreamKeyMaterial,
}

impl MediaStreamConfiguration {
    /// Total wire length of the offer message.
    pub fn wire_len(&self) -> Result<usize> {
        // Native layout (confirmed from `_RFBMediaStreamServerConfiguration`):
        //   0x80 fixed header/keys + audio offer + 2x46 keys + video1 offer
        //   (+ video2: 2x46 keys + video2 offer when present).
        let audio_len = self.audio_offer.len();
        let video1_len = self.video1_offer.len();
        let video2_extra = match &self.video2_offer {
            Some(offer) => 2 * MEDIA_STREAM_KEY_LEN + offer.len(),
            None => 0,
        };
        let total = 0x80_usize
            .checked_add(audio_len)
            .and_then(|v| v.checked_add(2 * MEDIA_STREAM_KEY_LEN))
            .and_then(|v| v.checked_add(video1_len))
            .and_then(|v| v.checked_add(video2_extra))
            .ok_or(Error::LimitExceeded("media stream message"))?;
        if total <= MAX_MEDIA_STREAM_MESSAGE {
            Ok(total)
        } else {
            Err(Error::LimitExceeded("media stream message"))
        }
    }

    /// Serialize the offer. Layout mirrors `_RFBMediaStreamServerConfiguration`
    /// in ScreenSharing.framework (macOS 26.6 build 25G72):
    ///
    /// | offset | size | field |
    /// | ---: | ---: | --- |
    /// | `0x00` | 1 | message type `0x1c` |
    /// | `0x02` | 2 | BE messageSize (total - 4) |
    /// | `0x04` | 2 | BE messageVersion (`0x0300`) |
    /// | `0x06` | 4 | BE flags |
    /// | `0x0a` | 2 | BE audio offer length |
    /// | `0x0c` | 2 | BE video1 offer length |
    /// | `0x0e` | 2 | BE video2 offer length (0 when absent) |
    /// | `0x10` | 4 | reserved, zero |
    /// | `0x14` | 16 | session UUID |
    /// | `0x24` | 46 | audio key viewer→server |
    /// | `0x52` | 46 | audio key server→viewer |
    /// | `0x80` | n | audio offer (binary plist) |
    /// | +n | 46 | video1 key viewer→server |
    /// | +0x2e | 46 | video1 key server→viewer |
    /// | +0x5c | m | video1 offer (binary plist) |
    /// | +m | 92 | video2 keys (viewer→server, server→viewer) |
    /// | +0x5c | k | video2 offer (only when present) |
    pub fn encode(&self) -> Result<Vec<u8>> {
        let total = self.wire_len()?;
        let mut out = vec![0u8; total];
        out[0] = CLIENT_MEDIA_STREAM_MESSAGE_TYPE;
        let size_field =
            u16::try_from(total - 4).map_err(|_| Error::LimitExceeded("message size"))?;
        out[2..4].copy_from_slice(&size_field.to_be_bytes());
        out[4..6].copy_from_slice(&self.message_version.to_be_bytes());
        out[6..10].copy_from_slice(&self.flags.raw().to_be_bytes());

        let audio_len = u16::try_from(self.audio_offer.len())
            .map_err(|_| Error::LimitExceeded("audio offer length"))?;
        let video1_len = u16::try_from(self.video1_offer.len())
            .map_err(|_| Error::LimitExceeded("video1 offer length"))?;
        let video2_len = match &self.video2_offer {
            Some(offer) => u16::try_from(offer.len())
                .map_err(|_| Error::LimitExceeded("video2 offer length"))?,
            None => 0,
        };
        out[0xa..0xc].copy_from_slice(&audio_len.to_be_bytes());
        out[0xc..0xe].copy_from_slice(&video1_len.to_be_bytes());
        out[0xe..0x10].copy_from_slice(&video2_len.to_be_bytes());

        out[SESSION_ID_OFFSET..SESSION_ID_OFFSET + SESSION_ID_LEN]
            .copy_from_slice(&self.session_id);

        // Two leading keys, then audio offer, two video1 keys, video1 offer,
        // then (optionally) video2 keys and offer.
        let mut pos = 0x24;
        pos = write_key(&mut out, pos, &self.keys.audio_viewer_to_server)?;
        pos = write_key(&mut out, pos, &self.keys.audio_server_to_viewer)?;
        out[pos..pos + self.audio_offer.len()].copy_from_slice(&self.audio_offer);
        pos += self.audio_offer.len();
        pos = write_key(&mut out, pos, &self.keys.video1_viewer_to_server)?;
        pos = write_key(&mut out, pos, &self.keys.video1_server_to_viewer)?;
        out[pos..pos + self.video1_offer.len()].copy_from_slice(&self.video1_offer);
        pos += self.video1_offer.len();
        if let Some(video2) = &self.video2_offer {
            let v2s = self
                .keys
                .video2_viewer_to_server
                .ok_or(Error::Invalid("video2 key missing"))?;
            let s2v = self
                .keys
                .video2_server_to_viewer
                .ok_or(Error::Invalid("video2 key missing"))?;
            pos = write_key(&mut out, pos, &v2s)?;
            pos = write_key(&mut out, pos, &s2v)?;
            out[pos..pos + video2.len()].copy_from_slice(video2);
            pos += video2.len();
        }
        debug_assert_eq!(pos, total);
        Ok(out)
    }

    /// Parse an offer. `bytes` must contain the complete message starting at
    /// the type byte. Returns the message and the number of consumed bytes.
    pub fn parse(bytes: &[u8]) -> Result<(Self, usize)> {
        if bytes.len() < HEADER_LEN + SESSION_ID_LEN {
            return Err(Error::NeedMore {
                needed: HEADER_LEN + SESSION_ID_LEN,
                available: bytes.len(),
            });
        }
        if bytes[0] != CLIENT_MEDIA_STREAM_MESSAGE_TYPE {
            return Err(Error::Invalid(
                "expected media stream client message type 0x1c",
            ));
        }
        let size_field = u16::from_be_bytes([bytes[2], bytes[3]]);
        let version = u16::from_be_bytes([bytes[4], bytes[5]]);
        let flags = MediaStreamFlags(u32::from_be_bytes(bytes[6..10].try_into().expect("slice")));
        let audio_offer_len = u16::from_be_bytes([bytes[0xa], bytes[0xb]]) as usize;
        let video1_offer_len = u16::from_be_bytes([bytes[0xc], bytes[0xd]]) as usize;
        let video2_offer_len = u16::from_be_bytes([bytes[0xe], bytes[0xf]]) as usize;

        // messageSize counts everything after the 4-byte type/version prefix.
        let body_len = usize::from(size_field)
            .checked_add(4)
            .ok_or(Error::LimitExceeded("media stream message size"))?;
        if body_len > bytes.len() {
            return Err(Error::NeedMore {
                needed: body_len,
                available: bytes.len(),
            });
        }

        let session_id: [u8; SESSION_ID_LEN] = bytes
            [SESSION_ID_OFFSET..SESSION_ID_OFFSET + SESSION_ID_LEN]
            .try_into()
            .expect("slice checked");

        let has_video2 = video2_offer_len != 0;
        let audio_v2s = read_key(bytes, 0x24)?;
        let audio_s2v = read_key(bytes, 0x52)?;
        let mut pos = 0x80usize;
        let audio_offer = read_slice(bytes, pos, audio_offer_len)?;
        pos += audio_offer_len;
        let video1_v2s = read_key(bytes, pos)?;
        pos += MEDIA_STREAM_KEY_LEN;
        let video1_s2v = read_key(bytes, pos)?;
        pos += MEDIA_STREAM_KEY_LEN;
        let video1_offer = read_slice(bytes, pos, video1_offer_len)?;
        pos += video1_offer_len;
        let (video2_v2s, video2_s2v, video2_offer) = if has_video2 {
            let v2s = read_key(bytes, pos)?;
            pos += MEDIA_STREAM_KEY_LEN;
            let s2v = read_key(bytes, pos)?;
            pos += MEDIA_STREAM_KEY_LEN;
            (
                Some(v2s),
                Some(s2v),
                Some(read_slice(bytes, pos, video2_offer_len)?),
            )
        } else {
            (None, None, None)
        };

        let expected = 0x80usize
            .checked_add(audio_offer_len)
            .and_then(|v| v.checked_add(2 * MEDIA_STREAM_KEY_LEN))
            .and_then(|v| v.checked_add(video1_offer_len))
            .and_then(|v| {
                if has_video2 {
                    v.checked_add(2 * MEDIA_STREAM_KEY_LEN)
                        .and_then(|v| v.checked_add(video2_offer_len))
                } else {
                    Some(v)
                }
            })
            .ok_or(Error::LimitExceeded("media stream message size"))?;
        if body_len != expected {
            return Err(Error::Invalid(
                "media stream message length does not match fixed payload layout",
            ));
        }

        Ok((
            Self {
                message_version: version,
                flags,
                session_id,
                audio_offer: audio_offer.to_vec(),
                video1_offer: video1_offer.to_vec(),
                video2_offer: video2_offer.map(<[u8]>::to_vec),
                keys: MediaStreamKeyMaterial {
                    audio_viewer_to_server: audio_v2s,
                    audio_server_to_viewer: audio_s2v,
                    video1_viewer_to_server: video1_v2s,
                    video1_server_to_viewer: video1_s2v,
                    video2_viewer_to_server: video2_v2s,
                    video2_server_to_viewer: video2_s2v,
                },
            },
            body_len,
        ))
    }
}

/// Server reply "RFBMediaStreamMessage1" (message type `0x23`): confirms the
/// AVC encoding and hands out the UDP ports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaStreamMessage1 {
    pub encoding: i32,
    pub video1_port: u16,
    pub video2_port: Option<u16>,
    pub audio_port: Option<u16>,
    pub video1_hdr: bool,
    pub video2_hdr: bool,
    pub stream_count: u16,
}

const MESSAGE1_STRUCT_LEN: usize = 0x44;
/// The server's framebuffer-update path sends only the tail of the native
/// 0x44-byte message1 allocation. The preceding 0x1e bytes are the RFB
/// rectangle/message envelope, so the payload seen by the decoder is 0x26
/// bytes and starts with `00 24 00 01`.
const MESSAGE1_COMPACT_TAIL_LEN: usize = 0x26;
const MESSAGE1_ENCODING_OFFSET: usize = 0x1a;
const MESSAGE1_PORT1_OFFSET: usize = 0x28;
const MESSAGE1_PORT2_OFFSET: usize = 0x2e;
const MESSAGE1_PORT3_OFFSET: usize = 0x34;

impl MediaStreamMessage1 {
    /// Wire size of the fixed Message1 structure, excluding an RFB rectangle
    /// header or a surrounding server-message type byte.
    pub const WIRE_LEN: usize = MESSAGE1_STRUCT_LEN;

    /// Parse the 68-byte struct produced by `EncodeRFBMediaStreamMessage1`.
    ///
    /// The server sends this as RFB message type `0x23`; pass the body that
    /// follows the type byte. A 16-byte record header (8 zero bytes, `u32`
    /// length, `u16` zero, `u16` BE length) is tolerated when present.
    pub fn parse(body: &[u8]) -> Result<Self> {
        Self::parse_with_len(body).map(|(message, _)| message)
    }

    /// Parse Message1 and return the number of bytes consumed from `body`.
    /// The compact form used by older Screen Sharing builds is shorter than
    /// the fixed native structure, so stream framers must use this method.
    pub fn parse_with_len(body: &[u8]) -> Result<(Self, usize)> {
        if is_compact_message1_tail(body) {
            return Ok((Self::parse_compact_tail(body)?, MESSAGE1_COMPACT_TAIL_LEN));
        }
        let (base, base_offset) = if body.len() >= MESSAGE1_STRUCT_LEN
            && u32::from_be_bytes(
                body[MESSAGE1_ENCODING_OFFSET..MESSAGE1_ENCODING_OFFSET + 4]
                    .try_into()
                    .expect("slice"),
            ) == super::ENCODING_AVC_MEDIA_STREAM as u32
        {
            (body, 0)
        } else if body.len() >= MESSAGE1_STRUCT_LEN + 0x10
            && u32::from_be_bytes(
                body[MESSAGE1_ENCODING_OFFSET + 0x10..MESSAGE1_ENCODING_OFFSET + 0x14]
                    .try_into()
                    .expect("slice"),
            ) == super::ENCODING_AVC_MEDIA_STREAM as u32
        {
            (&body[0x10..], 0x10)
        } else if body.len() >= 0x2c
            && u32::from_be_bytes(body[0x0c..0x10].try_into().expect("slice"))
                == super::ENCODING_AVC_MEDIA_STREAM as u32
        {
            return Ok((Self::parse_compact(body)?, 0x2c));
        } else if body.len() < MESSAGE1_STRUCT_LEN {
            return Err(Error::NeedMore {
                needed: MESSAGE1_STRUCT_LEN,
                available: body.len(),
            });
        } else {
            return Err(Error::Invalid(
                "media stream message1 missing AVC encoding marker",
            ));
        };
        if base.len() < MESSAGE1_STRUCT_LEN {
            return Err(Error::NeedMore {
                needed: MESSAGE1_STRUCT_LEN,
                available: base.len(),
            });
        }
        let encoding = u32::from_be_bytes(
            base[MESSAGE1_ENCODING_OFFSET..MESSAGE1_ENCODING_OFFSET + 4]
                .try_into()
                .expect("slice"),
        ) as i32;
        if encoding != super::ENCODING_AVC_MEDIA_STREAM {
            return Err(Error::Invalid("not an AVC media stream message"));
        }
        let audio_port = u16::from_be_bytes(
            base[MESSAGE1_PORT1_OFFSET..MESSAGE1_PORT1_OFFSET + 2]
                .try_into()
                .expect("slice"),
        );
        let video1_port = u16::from_be_bytes(
            base[MESSAGE1_PORT2_OFFSET..MESSAGE1_PORT2_OFFSET + 2]
                .try_into()
                .expect("slice"),
        );
        let video2_port = u16::from_be_bytes(
            base[MESSAGE1_PORT3_OFFSET..MESSAGE1_PORT3_OFFSET + 2]
                .try_into()
                .expect("slice"),
        );
        let port2_flags = u32::from_be_bytes(base[0x30..0x34].try_into().expect("slice"));
        let port3_flags = u32::from_be_bytes(base[0x36..0x3a].try_into().expect("slice"));
        let stream_count = u16::from_be_bytes([base[2], base[3]]);
        Ok((
            Self {
                encoding,
                video1_port,
                video2_port: (video2_port != 0).then_some(video2_port),
                audio_port: (audio_port != 0).then_some(audio_port),
                video1_hdr: port2_flags & 0x0200_0000 != 0,
                video2_hdr: port3_flags & 0x0200_0000 != 0,
                stream_count,
            },
            base_offset + MESSAGE1_STRUCT_LEN,
        ))
    }

    fn parse_compact(body: &[u8]) -> Result<Self> {
        if body.len() < 0x2c {
            return Err(Error::NeedMore {
                needed: 0x2c,
                available: body.len(),
            });
        }
        let encoding = u32::from_be_bytes(body[0x0c..0x10].try_into().expect("slice")) as i32;
        let audio_port = u16::from_be_bytes(body[0x1a..0x1c].try_into().expect("slice"));
        let video1_port = u16::from_be_bytes(body[0x20..0x22].try_into().expect("slice"));
        let video2_port = u16::from_be_bytes(body[0x26..0x28].try_into().expect("slice"));
        let port2_flags = u32::from_be_bytes(body[0x22..0x26].try_into().expect("slice"));
        let port3_flags = u32::from_be_bytes(body[0x28..0x2c].try_into().expect("slice"));
        Ok(Self {
            encoding,
            video1_port,
            video2_port: (video2_port != 0).then_some(video2_port),
            audio_port: (audio_port != 0).then_some(audio_port),
            video1_hdr: port2_flags & 0x0200_0000 != 0,
            video2_hdr: port3_flags & 0x0200_0000 != 0,
            stream_count: 1,
        })
    }

    fn parse_compact_tail(body: &[u8]) -> Result<Self> {
        let audio_port = u16::from_be_bytes(body[0x0a..0x0c].try_into().expect("tail checked"));
        let video1_port = u16::from_be_bytes(body[0x10..0x12].try_into().expect("tail checked"));
        let video2_port = u16::from_be_bytes(body[0x16..0x18].try_into().expect("tail checked"));
        let video1_flags = u32::from_be_bytes(body[0x0c..0x10].try_into().expect("tail checked"));
        let video2_flags = u32::from_be_bytes(body[0x12..0x16].try_into().expect("tail checked"));
        Ok(Self {
            encoding: super::ENCODING_AVC_MEDIA_STREAM,
            video1_port,
            video2_port: (video2_port != 0).then_some(video2_port),
            audio_port: (audio_port != 0).then_some(audio_port),
            video1_hdr: video1_flags & 0x0200_0000 != 0,
            video2_hdr: video2_flags & 0x0200_0000 != 0,
            stream_count: u16::from_be_bytes(body[0x02..0x04].try_into().expect("tail checked")),
        })
    }

    /// Serialize the 68-byte struct (tests and server oracle).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![0u8; MESSAGE1_STRUCT_LEN];
        out[0x8..0xc].copy_from_slice(&0x36u32.to_be_bytes());
        out[0xe..0x12].copy_from_slice(&0x0100_0000u32.to_be_bytes());
        out[0x1a..0x1e].copy_from_slice(&(super::ENCODING_AVC_MEDIA_STREAM as u32).to_be_bytes());
        out[0x1e..0x22].copy_from_slice(&0x0100_2400u32.to_be_bytes());
        out[0x22..0x24].copy_from_slice(&0x0100u16.to_be_bytes());
        if let Some(audio) = self.audio_port {
            out[0x28..0x2a].copy_from_slice(&audio.to_be_bytes());
        }
        out[0x2a..0x2e].copy_from_slice(&0x0100_0000u32.to_be_bytes());
        out[0x2e..0x30].copy_from_slice(&self.video1_port.to_be_bytes());
        let p2flags = 0x0100_0000u32 | if self.video1_hdr { 0x0200_0000 } else { 0 };
        out[0x30..0x34].copy_from_slice(&p2flags.to_be_bytes());
        if let Some(video2) = self.video2_port {
            out[0x34..0x36].copy_from_slice(&video2.to_be_bytes());
        }
        let p3flags = if self.video2_port.is_some() {
            0x0100_0000u32 | if self.video2_hdr { 0x0200_0000 } else { 0 }
        } else {
            0
        };
        out[0x36..0x3a].copy_from_slice(&p3flags.to_be_bytes());
        out
    }
}

fn is_compact_message1_tail(body: &[u8]) -> bool {
    body.len() >= MESSAGE1_COMPACT_TAIL_LEN
        && body[0..2] == [0x00, 0x24]
        && body[2..4] == [0x00, 0x01]
        && u16::from_be_bytes(body[0x0a..0x0c].try_into().expect("tail checked")) != 0
}

/// Server reply "RFBMediaStreamMessage2" (answer). The negotiator answer body
/// is opaque; only the confirmed fields are decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaStreamAnswer {
    pub flags: u32,
    pub field_a: u16,
    pub field_b: u16,
    pub field_c: u16,
    pub answer_body: Vec<u8>,
}

impl MediaStreamAnswer {
    /// Parse the answer struct produced by `EncodeRFBMediaStreamAnswer`.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        const ANSWER_BODY_OFFSET: usize = 0x2e;
        if bytes.len() < ANSWER_BODY_OFFSET {
            return Err(Error::NeedMore {
                needed: ANSWER_BODY_OFFSET,
                available: bytes.len(),
            });
        }
        let encoding = u32::from_be_bytes(bytes[0x1a..0x1e].try_into().expect("slice"));
        if encoding != super::ENCODING_AVC_MEDIA_STREAM as u32 {
            return Err(Error::Invalid("answer missing AVC encoding marker"));
        }
        let body_len = usize::from(u16::from_be_bytes(
            bytes[0x1e..0x20].try_into().expect("slice"),
        ));
        let total = ANSWER_BODY_OFFSET
            .checked_add(body_len)
            .ok_or(Error::LimitExceeded("media stream answer"))?;
        if bytes.len() < total {
            return Err(Error::NeedMore {
                needed: total,
                available: bytes.len(),
            });
        }
        let flags = u32::from_be_bytes(bytes[0x24..0x28].try_into().expect("slice"));
        let field_a = u16::from_be_bytes(bytes[0x28..0x2a].try_into().expect("slice"));
        let field_b = u16::from_be_bytes(bytes[0x2a..0x2c].try_into().expect("slice"));
        let field_c = u16::from_be_bytes(bytes[0x2c..0x2e].try_into().expect("slice"));
        Ok(Self {
            flags,
            field_a,
            field_b,
            field_c,
            answer_body: bytes[ANSWER_BODY_OFFSET..total].to_vec(),
        })
    }

    fn parse_compact_with_len(bytes: &[u8]) -> Result<(Self, usize)> {
        if bytes.len() < 0x12 {
            return Err(Error::NeedMore {
                needed: 0x12,
                available: bytes.len(),
            });
        }
        let body_len = usize::from(u16::from_be_bytes(
            bytes[0..2].try_into().expect("slice checked"),
        ));
        let total = body_len
            .checked_add(2)
            .ok_or(Error::LimitExceeded("compact media stream answer"))?;
        if body_len < 0x10 {
            return Err(Error::Invalid("compact media stream answer is too short"));
        }
        if bytes.len() < total {
            return Err(Error::NeedMore {
                needed: total,
                available: bytes.len(),
            });
        }
        if bytes[2..6] != [0x00, 0x02, 0x00, 0x02] {
            return Err(Error::Invalid("compact media stream answer discriminator"));
        }
        Ok((
            Self {
                flags: u32::from_be_bytes(bytes[0x06..0x0a].try_into().expect("slice")),
                field_a: u16::from_be_bytes(bytes[0x0a..0x0c].try_into().expect("slice")),
                field_b: u16::from_be_bytes(bytes[0x0c..0x0e].try_into().expect("slice")),
                field_c: u16::from_be_bytes(bytes[0x0e..0x10].try_into().expect("slice")),
                answer_body: bytes[0x10..total].to_vec(),
            },
            total,
        ))
    }

    /// Serialize the answer struct (tests and server oracle).
    pub fn encode(&self) -> Result<Vec<u8>> {
        let body_len = u16::try_from(self.answer_body.len())
            .map_err(|_| Error::LimitExceeded("answer body"))?;
        let total = 0x2e + usize::from(body_len);
        let mut out = vec![0u8; total];
        out[0x8..0xc].copy_from_slice(&(total as u32 - 0xc).to_be_bytes());
        out[0xe..0x12].copy_from_slice(&0x0100_0000u32.to_be_bytes());
        out[0x1a..0x1e].copy_from_slice(&(super::ENCODING_AVC_MEDIA_STREAM as u32).to_be_bytes());
        out[0x1e..0x20].copy_from_slice(&body_len.to_be_bytes());
        out[0x20..0x24].copy_from_slice(&0x0002_0002u32.to_be_bytes());
        out[0x24..0x28].copy_from_slice(&self.flags.to_be_bytes());
        out[0x28..0x2a].copy_from_slice(&self.field_a.to_be_bytes());
        out[0x2a..0x2c].copy_from_slice(&self.field_b.to_be_bytes());
        out[0x2c..0x2e].copy_from_slice(&self.field_c.to_be_bytes());
        out[0x2e..].copy_from_slice(&self.answer_body);
        Ok(out)
    }
}

/// Server error reply built by `EncodeRFBMediaStreamErrorMessage`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaStreamError {
    pub error_type: u8,
    pub error_sub_code: u8,
}

const MEDIA_STREAM_ERROR_COMPACT_LEN: usize = 0x12;

impl MediaStreamError {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 0x30 {
            return Err(Error::NeedMore {
                needed: 0x30,
                available: bytes.len(),
            });
        }
        if u32::from_be_bytes(bytes[0x1a..0x1e].try_into().expect("slice"))
            != super::ENCODING_AVC_MEDIA_STREAM as u32
        {
            return Err(Error::Invalid("error message missing AVC encoding marker"));
        }
        let flags = u32::from_be_bytes(bytes[0x1e..0x22].try_into().expect("slice"));
        Ok(Self {
            error_type: (flags >> 16) as u8,
            error_sub_code: (flags >> 8) as u8,
        })
    }

    fn parse_compact(bytes: &[u8]) -> Result<(Self, usize)> {
        if bytes.len() < MEDIA_STREAM_ERROR_COMPACT_LEN {
            return Err(Error::NeedMore {
                needed: MEDIA_STREAM_ERROR_COMPACT_LEN,
                available: bytes.len(),
            });
        }
        if bytes[0..2] != [0x00, 0x10] || bytes[2..4] != [0x00, 0x03] || bytes[4..6] != [0x00, 0x01]
        {
            return Err(Error::Invalid("invalid compact AVC media stream error"));
        }
        let error_type = u32::from_be_bytes(bytes[0x0a..0x0e].try_into().expect("slice"));
        let error_sub_code = u32::from_be_bytes(bytes[0x0e..0x12].try_into().expect("slice"));
        let error_type = u8::try_from(error_type)
            .map_err(|_| Error::Invalid("compact AVC media stream error type is too large"))?;
        let error_sub_code = u8::try_from(error_sub_code)
            .map_err(|_| Error::Invalid("compact AVC media stream error subcode is too large"))?;
        Ok((
            Self {
                error_type,
                error_sub_code,
            },
            MEDIA_STREAM_ERROR_COMPACT_LEN,
        ))
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![0u8; 0x30];
        out[0x8..0xc].copy_from_slice(&0x22u32.to_be_bytes());
        out[0xe..0x12].copy_from_slice(&0x0100_0000u32.to_be_bytes());
        out[0x1a..0x1e].copy_from_slice(&(super::ENCODING_AVC_MEDIA_STREAM as u32).to_be_bytes());
        let flags = u32::from(self.error_type) << 16 | u32::from(self.error_sub_code) << 8;
        out[0x1e..0x22].copy_from_slice(&flags.to_be_bytes());
        out
    }
}

/// One of the three server-side media stream messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaStreamServerReply {
    Message1(MediaStreamMessage1),
    Answer(MediaStreamAnswer),
    Error(MediaStreamError),
}

impl MediaStreamServerReply {
    /// Parse the payload of a zero-sized framebuffer rectangle carrying an
    /// AVC server reply. Native Screen Sharing uses a compact 18-byte error
    /// record when it rejects the media configuration.
    pub fn parse_rectangle_payload_with_len(payload: &[u8]) -> Result<(Self, usize)> {
        if is_compact_media_stream_error(payload) {
            return MediaStreamError::parse_compact(payload)
                .map(|(error, consumed)| (Self::Error(error), consumed));
        }
        if is_compact_media_stream_answer(payload) {
            return MediaStreamAnswer::parse_compact_with_len(payload)
                .map(|(answer, consumed)| (Self::Answer(answer), consumed));
        }
        MediaStreamMessage1::parse_with_len(payload)
            .map(|(message, consumed)| (Self::Message1(message), consumed))
    }

    /// Classify a body by looking for the AVC marker and confirmed fields.
    pub fn parse(message_type: u8, body: &[u8]) -> Result<Self> {
        if message_type != SERVER_MEDIA_STREAM_MESSAGE_TYPE {
            return Err(Error::UnsupportedServerMessage(message_type));
        }
        let body = if body.len() >= 4
            && body[0] == 0
            && usize::from(u16::from_be_bytes([body[1], body[2]])) == body.len().saturating_sub(3)
        {
            &body[3..]
        } else {
            body
        };
        if is_compact_media_stream_error(body) {
            return MediaStreamError::parse_compact(body).map(|(error, _)| Self::Error(error));
        }
        if is_compact_media_stream_answer(body) {
            return MediaStreamAnswer::parse_compact_with_len(body)
                .map(|(answer, _)| Self::Answer(answer));
        }
        if is_compact_message1_tail(body) {
            return MediaStreamMessage1::parse(body).map(Self::Message1);
        }
        if body.len() >= 0x10
            && u32::from_be_bytes(body[0x0c..0x10].try_into().expect("slice"))
                == super::ENCODING_AVC_MEDIA_STREAM as u32
        {
            return MediaStreamMessage1::parse(body).map(Self::Message1);
        }
        let encoding_ok = body.len() >= 0x1e
            && u32::from_be_bytes(body[0x1a..0x1e].try_into().expect("slice"))
                == super::ENCODING_AVC_MEDIA_STREAM as u32;
        if !encoding_ok {
            return Err(Error::Invalid(
                "unrecognized AVC media stream server message",
            ));
        }

        // Message2 has its discriminator at 0x20 because 0x1e contains the
        // variable answer-body length. It must be checked before Message1:
        // both messages carry the same 1010 marker.
        if body.len() >= 0x24
            && u32::from_be_bytes(body[0x20..0x24].try_into().expect("slice")) == 0x0002_0002
        {
            return MediaStreamAnswer::parse(body).map(Self::Answer);
        }
        if body.len() >= 0x22
            && u32::from_be_bytes(body[0x1e..0x22].try_into().expect("slice")) == 0x0100_2400
        {
            return MediaStreamMessage1::parse(body).map(Self::Message1);
        }
        // The error builder has no separate discriminator; its fixed length
        // and AVC marker are the stable classification boundary.
        if body.len() == 0x30
            && body.len() >= 0x0c
            && u32::from_be_bytes(body[0x08..0x0c].try_into().expect("slice")) == 0x22
        {
            return MediaStreamError::parse(body).map(Self::Error);
        }
        MediaStreamMessage1::parse(body).map(Self::Message1)
    }

    /// Parse a complete raw server media-stream message, including its `0x23`
    /// type byte. Both the fixed structure and the compact
    /// `[pad][u16 length][payload]` envelope used by older Screen Sharing
    /// builds are accepted.
    pub fn parse_framed(bytes: &[u8]) -> Result<(Self, usize)> {
        if bytes.first().copied() != Some(SERVER_MEDIA_STREAM_MESSAGE_TYPE) {
            return Err(Error::Invalid("expected AVC media stream server message"));
        }
        let body = bytes.get(1..).ok_or(Error::NeedMore {
            needed: 2,
            available: bytes.len(),
        })?;

        if is_compact_media_stream_error(body) {
            let (error, consumed) = MediaStreamError::parse_compact(body)?;
            return Ok((Self::Error(error), 1 + consumed));
        }
        if is_compact_media_stream_answer(body) {
            let (answer, consumed) = MediaStreamAnswer::parse_compact_with_len(body)?;
            return Ok((Self::Answer(answer), 1 + consumed));
        }

        // Compact envelope: [pad][u16 payload length][payload]. A fixed
        // Message1 also starts with zero bytes, so require a plausible
        // compact payload length before taking this branch.
        if body.len() >= 3
            && body[0] == 0
            && usize::from(u16::from_be_bytes([body[1], body[2]])) >= 0x12
        {
            let payload_len = usize::from(u16::from_be_bytes([body[1], body[2]]));
            let total = 1usize
                .checked_add(3)
                .and_then(|v| v.checked_add(payload_len))
                .ok_or(Error::LimitExceeded("media stream server message"))?;
            if bytes.len() < total {
                return Err(Error::NeedMore {
                    needed: total,
                    available: bytes.len(),
                });
            }
            let reply = Self::parse(SERVER_MEDIA_STREAM_MESSAGE_TYPE, &body[3..3 + payload_len])?;
            return Ok((reply, total));
        }

        // Answer length is stored before the discriminator and therefore
        // determines the boundary of this variable-sized raw message.
        if body.len() >= 0x24
            && u32::from_be_bytes(body[0x1a..0x1e].try_into().expect("slice"))
                == super::ENCODING_AVC_MEDIA_STREAM as u32
            && u32::from_be_bytes(body[0x20..0x24].try_into().expect("slice")) == 0x0002_0002
        {
            let answer_len = usize::from(u16::from_be_bytes(
                body[0x1e..0x20].try_into().expect("slice"),
            ));
            let total = 1usize
                .checked_add(0x2e)
                .and_then(|v| v.checked_add(answer_len))
                .ok_or(Error::LimitExceeded("media stream answer"))?;
            if bytes.len() < total {
                return Err(Error::NeedMore {
                    needed: total,
                    available: bytes.len(),
                });
            }
            let reply = Self::parse(SERVER_MEDIA_STREAM_MESSAGE_TYPE, &body[..total - 1])?;
            return Ok((reply, total));
        }

        // Error messages are fixed-width and have no variable payload.
        if body.len() >= 0x30
            && body.len() < MESSAGE1_STRUCT_LEN
            && u32::from_be_bytes(body[0x08..0x0c].try_into().expect("slice")) == 0x22
            && u32::from_be_bytes(body[0x1a..0x1e].try_into().expect("slice"))
                == super::ENCODING_AVC_MEDIA_STREAM as u32
        {
            let reply = Self::parse(SERVER_MEDIA_STREAM_MESSAGE_TYPE, &body[..0x30])?;
            return Ok((reply, 1 + 0x30));
        }

        if body.len() >= MESSAGE1_STRUCT_LEN {
            let reply = Self::parse(
                SERVER_MEDIA_STREAM_MESSAGE_TYPE,
                &body[..MESSAGE1_STRUCT_LEN],
            )?;
            return Ok((reply, 1 + MESSAGE1_STRUCT_LEN));
        }

        if body.len() < 4 {
            return Err(Error::NeedMore {
                needed: 5,
                available: bytes.len(),
            });
        }
        Err(Error::NeedMore {
            needed: 1 + MESSAGE1_STRUCT_LEN,
            available: bytes.len(),
        })
    }
}

fn is_compact_media_stream_error(bytes: &[u8]) -> bool {
    bytes.len() >= MEDIA_STREAM_ERROR_COMPACT_LEN
        && bytes[0..2] == [0x00, 0x10]
        && bytes[2..4] == [0x00, 0x03]
        && bytes[4..6] == [0x00, 0x01]
}

fn is_compact_media_stream_answer(bytes: &[u8]) -> bool {
    bytes.len() >= 0x12
        && usize::from(u16::from_be_bytes([bytes[0], bytes[1]])) >= 0x10
        && bytes[2..6] == [0x00, 0x02, 0x00, 0x02]
}

fn write_key(out: &mut [u8], pos: usize, key: &[u8; MEDIA_STREAM_KEY_LEN]) -> Result<usize> {
    let end = pos + MEDIA_STREAM_KEY_LEN;
    if end > out.len() {
        return Err(Error::LimitExceeded("media stream key position"));
    }
    out[pos..end].copy_from_slice(key);
    Ok(end)
}

fn read_key(bytes: &[u8], pos: usize) -> Result<[u8; MEDIA_STREAM_KEY_LEN]> {
    let end = pos
        .checked_add(MEDIA_STREAM_KEY_LEN)
        .ok_or(Error::LimitExceeded("key offset"))?;
    if end > bytes.len() {
        return Err(Error::NeedMore {
            needed: MEDIA_STREAM_KEY_LEN,
            available: bytes.len().saturating_sub(pos),
        });
    }
    Ok(bytes[pos..end].try_into().expect("length checked"))
}

fn read_slice(bytes: &[u8], pos: usize, len: usize) -> Result<&[u8]> {
    let end = pos
        .checked_add(len)
        .ok_or(Error::LimitExceeded("payload offset"))?;
    if end > bytes.len() {
        return Err(Error::NeedMore {
            needed: len,
            available: bytes.len().saturating_sub(pos),
        });
    }
    Ok(&bytes[pos..end])
}
