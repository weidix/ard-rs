//! Negotiation payloads carried inside the AVC media stream messages.
//!
//! The offers and answers are property-list blobs produced by
//! `AVCMediaStreamNegotiator` (AVConference.framework). The native builder
//! serializes them with `NSPropertyListBinaryFormat_v1_0` (format `0xc8`),
//! so they are compact binary plists rather than text SDP. The keys mirror
//! the FaceTime/AVConference negotiation vocabulary:
//!
//! * `CallID` (`kAVCMediaStreamOptionCallID`) - UUID string;
//! * `RemoteEndpointInfo` (`kAVCMediaStreamOptionRemoteEndpointInfo`) - the
//!   `VCCallInfoBlob` data;
//! * media mode, direction, and the SDP-style fields (`rtpmap`, `fmtp`,
//!   `profileLevelID`) that select H.264 vs HEVC and the profile/level.
//!
//! This module implements a small bounded binary-plist reader plus tolerant
//! extraction of the codec-relevant fields. It also accepts plain-text SDP
//! bodies so a future server variant that negotiates with classic SDP keeps
//! working without changes to the callers.

use crate::{Error, Result};

use super::MAX_MEDIA_STREAM_MESSAGE;

/// Maximum nesting depth accepted by the plist reader.
const MAX_PLIST_DEPTH: usize = 16;
/// Maximum number of objects in one plist.
const MAX_PLIST_OBJECTS: usize = 4096;

/// Video codec families understood by the media stream path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaStreamCodec {
    H264,
    Hevc,
}

impl MediaStreamCodec {
    pub fn name(self) -> &'static str {
        match self {
            Self::H264 => "H264",
            Self::Hevc => "HEVC",
        }
    }

    pub fn rtpmap(self, payload_type: u8) -> String {
        match self {
            Self::H264 => format!("rtpmap:{payload_type} H264/90000"),
            Self::Hevc => format!("rtpmap:{payload_type} HEVC/90000"),
        }
    }
}

/// Codec configuration extracted from a negotiation payload.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VideoCodecConfig {
    pub codec: Option<MediaStreamCodec>,
    pub payload_type: Option<u8>,
    pub profile_level_id: Option<String>,
    pub packetization_mode: Option<u8>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// Parsed form of a media-stream offer or answer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MediaStreamOffer {
    pub call_id: Option<String>,
    pub remote_endpoint_info: Option<Vec<u8>>,
    pub mode: Option<i64>,
    pub direction: Option<i64>,
    pub codec: VideoCodecConfig,
    /// Raw payload bytes as received from the variable-length plist slot.
    pub raw: Vec<u8>,
}

impl MediaStreamOffer {
    /// Parse a bounded binary-plist or SDP negotiation blob.
    pub fn parse(raw: &[u8]) -> Result<Self> {
        if raw.len() > MAX_MEDIA_STREAM_MESSAGE {
            return Err(Error::LimitExceeded("negotiation payload"));
        }
        let mut offer = Self {
            raw: raw.to_vec(),
            ..Self::default()
        };
        match parse_property_list(raw) {
            Ok(plist) => extract_plist_fields(&plist, &mut offer),
            Err(_error) if is_sdp_text(raw) => parse_sdp_text(raw, &mut offer.codec),
            Err(error) => return Err(error),
        }
        Ok(offer)
    }
}

fn extract_plist_fields(value: &PlistValue, offer: &mut MediaStreamOffer) {
    match value {
        PlistValue::Dict(entries) => {
            for (key, value) in entries {
                match key.as_str() {
                    "CallID" | "callID" => offer.call_id = value.as_string().map(str::to_owned),
                    "RemoteEndpointInfo" | "remoteEndpointInfo" => {
                        offer.remote_endpoint_info = value.as_data().map(<[u8]>::to_vec)
                    }
                    "mode" | "streamMode" | "mediaStreamMode" => offer.mode = value.as_integer(),
                    "direction" | "mediaStreamDirection" => offer.direction = value.as_integer(),
                    "rtpmap" | "RTPMap" => {
                        if let Some(text) = plist_text(value) {
                            offer.codec.codec = codec_from_rtpmap(text);
                            offer.codec.payload_type = payload_type_from_rtpmap(text);
                        }
                    }
                    "fmtp" | "FMTP" => {
                        if let Some(text) = plist_text(value) {
                            apply_fmtp(text, &mut offer.codec);
                        }
                    }
                    "codec" | "codecName" | "encodingName" => {
                        if let Some(text) = plist_text(value) {
                            offer.codec.codec = codec_from_rtpmap(text);
                        }
                    }
                    "sdp" | "SDP" => {
                        if let Some(text) = plist_text(value) {
                            parse_sdp_text(text.as_bytes(), &mut offer.codec);
                        }
                    }
                    "profileLevelID" | "profile-level-id" => {
                        offer.codec.profile_level_id = value.as_string().map(str::to_owned);
                    }
                    "payloadType" | "payload" => {
                        offer.codec.payload_type = value
                            .as_integer()
                            .and_then(|value| u8::try_from(value).ok());
                    }
                    "width" | "videoWidth" => {
                        offer.codec.width = value
                            .as_integer()
                            .and_then(|value| u32::try_from(value).ok());
                    }
                    "height" | "videoHeight" => {
                        offer.codec.height = value
                            .as_integer()
                            .and_then(|value| u32::try_from(value).ok());
                    }
                    _ => {}
                }
                extract_plist_fields(value, offer);
            }
        }
        PlistValue::Array(values) => {
            for value in values {
                extract_plist_fields(value, offer);
            }
        }
        _ => {}
    }
}

fn plist_text(value: &PlistValue) -> Option<&str> {
    match value {
        PlistValue::String(text) => Some(text),
        PlistValue::Data(data) => core::str::from_utf8(data).ok(),
        _ => None,
    }
}

/// Parse a negotiation payload and extract codec-relevant fields.
pub fn parse_negotiation_payload(raw: &[u8]) -> Result<VideoCodecConfig> {
    Ok(MediaStreamOffer::parse(raw)?.codec)
}

/// Protobuf `VCCallInfoBlob` fields used by `RemoteEndpointInfo`.
///
/// A live capture of the blob was: `08 00 10 01 1a 08 "Mac16,12"
/// 22 08 <device name> 2a 05 "25G72"` (31 bytes). Field numbers follow
/// `VCCallInfoBlob`; only the device type and build version are required by
/// the remote negotiator for a healthy session.
pub fn build_remote_endpoint_info(device_type: &str, build_version: &str) -> Vec<u8> {
    fn field_varint(field: u32, value: u64, out: &mut Vec<u8>) {
        let tag = field << 3;
        push_varint(tag as u64, out);
        push_varint(value, out);
    }
    fn field_bytes(field: u32, bytes: &[u8], out: &mut Vec<u8>) {
        let tag = (field << 3) | 2;
        push_varint(tag as u64, out);
        push_varint(bytes.len() as u64, out);
        out.extend_from_slice(bytes);
    }
    fn push_varint(mut value: u64, out: &mut Vec<u8>) {
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if value == 0 {
                break;
            }
        }
    }

    let mut blob = Vec::new();
    field_varint(1, 0, &mut blob); // version
    field_varint(2, 1, &mut blob); // client version marker
    field_bytes(3, device_type.as_bytes(), &mut blob); // device type
    field_bytes(5, build_version.as_bytes(), &mut blob); // OS/build version
    blob
}

/// Build the binary-plist media-stream offer for one stream.
///
/// The dictionary mirrors `AVCMediaStreamNegotiator -createOffer`:
/// `CallID` (a UUID), `RemoteEndpointInfo` (the `VCCallInfoBlob` data), plus
/// the stream mode and direction integers. The exact constant names for the
/// mode/direction keys live in AVConference's coalesced `__cstring` and could
/// not be resolved statically; the names below are best-effort and must be
/// validated against a live server (see `docs/SCREENSHARING_RE.md`).
pub fn build_media_stream_offer(
    call_id: &str,
    remote_endpoint_info: &[u8],
    mode: i64,
    direction: i64,
) -> Result<Vec<u8>> {
    let dict = PlistValueBuilder::Dict(vec![
        (
            "CallID".to_owned(),
            PlistValueBuilder::String(call_id.to_owned()),
        ),
        (
            "RemoteEndpointInfo".to_owned(),
            PlistValueBuilder::Data(remote_endpoint_info.to_vec()),
        ),
        (
            "mediaStreamMode".to_owned(),
            PlistValueBuilder::Integer(mode),
        ),
        (
            "mediaStreamDirection".to_owned(),
            PlistValueBuilder::Integer(direction),
        ),
    ]);
    serialize_property_list(&dict)
}

// ---------------------------------------------------------------------------
// Binary plist writer for building negotiator offers
// ---------------------------------------------------------------------------

/// A minimal binary-plist value used by the offer builder.
#[derive(Debug, Clone)]
enum PlistValueBuilder {
    String(String),
    Integer(i64),
    Data(Vec<u8>),
    Dict(Vec<(String, PlistValueBuilder)>),
}

/// Serialize a value with the given object-reference table.
///
/// Returns the object's index after appending it (and any nested objects) to
/// the object table, mirroring the native `NSPropertyListBinaryFormat_v1_0`
/// writer used by `AVCMediaStreamNegotiator -createOffer`.
fn append_plist_object(
    table: &mut Vec<u8>,
    offsets: &mut Vec<usize>,
    value: &PlistValueBuilder,
    object_ref_size: usize,
) -> Result<usize> {
    match value {
        PlistValueBuilder::String(text) => {
            let index = offsets.len();
            offsets.push(table.len());
            let len = text.len();
            if len >= 0x0f {
                if len > u32::MAX as usize {
                    return Err(Error::LimitExceeded("plist string length"));
                }
                table.push(0x5f);
                if len <= u8::MAX as usize {
                    table.push(0x10); // 1-byte length
                    table.push(len as u8);
                } else if len <= u16::MAX as usize {
                    table.push(0x11); // 2-byte length
                    table.extend_from_slice(&(len as u16).to_be_bytes());
                } else {
                    table.push(0x12); // 4-byte length
                    table.extend_from_slice(&(len as u32).to_be_bytes());
                }
            } else {
                table.push(0x50 | len as u8);
            }
            table.extend_from_slice(text.as_bytes());
            Ok(index)
        }
        PlistValueBuilder::Integer(value) => {
            let index = offsets.len();
            offsets.push(table.len());
            let bytes = value.to_be_bytes();
            let significant = bytes.iter().position(|b| *b != 0).unwrap_or(8);
            let width = (8 - significant).max(1);
            let marker = match width {
                1 => 0x10,
                2 => 0x11,
                4 => 0x12,
                8 => 0x13,
                _ => return Err(Error::Invalid("unsupported plist integer width")),
            };
            table.push(marker);
            table.extend_from_slice(&bytes[8 - width..]);
            Ok(index)
        }
        PlistValueBuilder::Data(data) => {
            let index = offsets.len();
            offsets.push(table.len());
            let len = data.len();
            if len >= 0x0f {
                if len > u32::MAX as usize {
                    return Err(Error::LimitExceeded("plist data length"));
                }
                table.push(0x4f);
                if len <= u8::MAX as usize {
                    table.push(0x10);
                    table.push(len as u8);
                } else if len <= u16::MAX as usize {
                    table.push(0x11);
                    table.extend_from_slice(&(len as u16).to_be_bytes());
                } else {
                    table.push(0x12);
                    table.extend_from_slice(&(len as u32).to_be_bytes());
                }
            } else {
                table.push(0x40 | len as u8);
            }
            table.extend_from_slice(data);
            Ok(index)
        }
        PlistValueBuilder::Dict(entries) => {
            let count = entries.len();
            if count > 0x0f {
                return Err(Error::LimitExceeded("plist dict size"));
            }
            let mut key_indices = Vec::with_capacity(count);
            let mut value_indices = Vec::with_capacity(count);
            for (key, val) in entries {
                let key_index = append_plist_object(
                    table,
                    offsets,
                    &PlistValueBuilder::String(key.clone()),
                    object_ref_size,
                )?;
                key_indices.push(key_index);
                let val_index = append_plist_object(table, offsets, val, object_ref_size)?;
                value_indices.push(val_index);
            }
            let index = offsets.len();
            offsets.push(table.len());
            table.push(0xd0 | count as u8);
            // Binary plist dictionaries store all key references first and
            // all value references second (the two arrays are parallel).
            for key_reference in key_indices {
                append_ref(table, key_reference, object_ref_size);
            }
            for value_reference in value_indices {
                append_ref(table, value_reference, object_ref_size);
            }
            Ok(index)
        }
    }
}

fn append_ref(table: &mut Vec<u8>, index: usize, width: usize) {
    let bytes = (index as u64).to_be_bytes();
    table.extend_from_slice(&bytes[8 - width..]);
}

/// Serialize a top-level plist value as `bplist00` bytes.
fn serialize_property_list(value: &PlistValueBuilder) -> Result<Vec<u8>> {
    let mut table = Vec::new();
    let mut offsets = Vec::new();
    // Object references are 1 byte for compact dictionaries; the native
    // writer uses the minimum width that fits the object count.
    let object_ref_size = 1usize;
    let top = append_plist_object(&mut table, &mut offsets, value, object_ref_size)?;
    let object_count = offsets.len();
    if object_count > 0xff {
        return Err(Error::LimitExceeded("plist object ref width"));
    }
    let mut out = Vec::with_capacity(table.len() + object_count + 32 + 8);
    out.extend_from_slice(b"bplist00");
    out.extend_from_slice(&table);
    let table_offset = out.len();
    let offset_size = 1usize;
    for &offset in &offsets {
        // Native plist offsets are relative to the start of the file, which
        // includes the 8-byte "bplist00" header.
        let bytes = ((offset + 8) as u64).to_be_bytes();
        out.extend_from_slice(&bytes[8 - offset_size..]);
    }
    let mut trailer = [0u8; 32];
    trailer[6] = offset_size as u8;
    trailer[7] = object_ref_size as u8;
    trailer[8..16].copy_from_slice(&(object_count as u64).to_be_bytes());
    trailer[16..24].copy_from_slice(&(top as u64).to_be_bytes());
    trailer[24..32].copy_from_slice(&(table_offset as u64).to_be_bytes());
    out.extend_from_slice(&trailer);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Binary plist reader
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PlistValue {
    String(String),
    Integer(i64),
    Data(Vec<u8>),
    Array(Vec<PlistValue>),
    Dict(Vec<(String, PlistValue)>),
    Boolean(bool),
    Null,
}

impl PlistValue {
    fn as_string(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    fn as_integer(&self) -> Option<i64> {
        match self {
            Self::Integer(value) => Some(*value),
            _ => None,
        }
    }

    fn as_data(&self) -> Option<&[u8]> {
        match self {
            Self::Data(value) => Some(value),
            _ => None,
        }
    }

    #[cfg(test)]
    fn as_dict(&self) -> Option<&[(String, PlistValue)]> {
        match self {
            Self::Dict(value) => Some(value),
            _ => None,
        }
    }
}

/// Parse a binary plist (`bplist00`).
pub(crate) fn parse_property_list(raw: &[u8]) -> Result<PlistValue> {
    if raw.starts_with(b"bplist00") {
        return parse_binary_plist(raw);
    }
    if raw.starts_with(b"<?xml") || raw.starts_with(b"<plist") {
        // A minimal XML fallback is deliberately not implemented; callers that
        // only need codec fields can use the text-SDP fallback below.
        return Err(Error::Invalid(
            "XML plist negotiation payload is unsupported",
        ));
    }
    Err(Error::Invalid("negotiation payload is not a plist"))
}

fn parse_binary_plist(raw: &[u8]) -> Result<PlistValue> {
    const TRAILER_LEN: usize = 32;
    if raw.len() < 8 + TRAILER_LEN {
        return Err(Error::NeedMore {
            needed: 8 + TRAILER_LEN,
            available: raw.len(),
        });
    }
    let trailer = &raw[raw.len() - TRAILER_LEN..];
    // trailer: 6 bytes unused, offsetSize, objectRefSize, numObjects (8),
    // topObject (8), offsetTableOffset (8).
    let offset_size = usize::from(trailer[6]);
    let object_ref_size = usize::from(trailer[7]);
    let object_count = read_be_usize(&trailer[8..16], 8)?;
    let top_object = read_be_usize(&trailer[16..24], 8)?;
    let table_offset = read_be_usize(&trailer[24..32], 8)?;
    if object_count == 0 || object_count > MAX_PLIST_OBJECTS {
        return Err(Error::LimitExceeded("plist object count"));
    }
    if offset_size == 0 || offset_size > 8 || object_ref_size == 0 || object_ref_size > 8 {
        return Err(Error::Invalid("plist size fields"));
    }
    let table_len = object_count
        .checked_mul(offset_size)
        .ok_or(Error::LimitExceeded("plist offset table"))?;
    let table_end = table_offset
        .checked_add(table_len)
        .ok_or(Error::LimitExceeded("plist offset table"))?;
    if table_end > raw.len() - TRAILER_LEN {
        return Err(Error::NeedMore {
            needed: table_end,
            available: raw.len(),
        });
    }
    let mut offsets = Vec::with_capacity(object_count);
    for i in 0..object_count {
        let start = table_offset + i * offset_size;
        offsets.push(read_be_usize(
            &raw[start..start + offset_size],
            offset_size,
        )?);
    }
    for &offset in &offsets {
        if offset >= raw.len() - TRAILER_LEN {
            return Err(Error::Invalid("plist object offset out of range"));
        }
    }
    parse_plist_object(raw, &offsets, top_object, 0)
}

fn parse_plist_object(
    raw: &[u8],
    offsets: &[usize],
    index: usize,
    depth: usize,
) -> Result<PlistValue> {
    if depth > MAX_PLIST_DEPTH {
        return Err(Error::LimitExceeded("plist nesting depth"));
    }
    let offset = *offsets
        .get(index)
        .ok_or(Error::Invalid("plist object reference"))?;
    let marker = *raw.get(offset).ok_or(Error::Invalid("plist marker"))?;
    let object_type = marker >> 4;
    let info = usize::from(marker & 0x0f);
    let pos = offset + 1;
    match object_type {
        0x0 => {
            if info == 0 {
                Ok(PlistValue::Null)
            } else if info == 8 {
                Ok(PlistValue::Boolean(false))
            } else if info == 9 {
                Ok(PlistValue::Boolean(true))
            } else {
                Err(Error::Invalid("plist simple object"))
            }
        }
        0x1 => {
            // Integer; info is the byte length (1 << info for info > 3).
            let len = if info < 4 { 1usize << info } else { info };
            let value = read_be_usize(&raw[pos..], len)?;
            Ok(PlistValue::Integer(value as i64))
        }
        0x2 => {
            // Real; not needed by the codec extractor.
            let len = if info < 3 { 1usize << info } else { info };
            let _ = read_be_usize(&raw[pos..], len)?;
            Ok(PlistValue::Null)
        }
        0x3 => {
            // Date (8-byte real); unused by the codec extractor.
            let _ = read_be_usize(&raw[pos..], 8)?;
            Ok(PlistValue::Null)
        }
        0x4 => {
            let (len, field_len) = read_length(raw, pos, info)?;
            let start = pos + field_len;
            Ok(PlistValue::Data(
                raw.get(start..start + len)
                    .ok_or(Error::Invalid("plist data range"))?
                    .to_vec(),
            ))
        }
        0x5 => {
            let (len, field_len) = read_length(raw, pos, info)?;
            let start = pos + field_len;
            let text = std::str::from_utf8(
                raw.get(start..start + len)
                    .ok_or(Error::Invalid("plist string range"))?,
            )
            .map_err(|_| Error::Invalid("plist string encoding"))?;
            Ok(PlistValue::String(text.to_owned()))
        }
        0x6 => {
            let (len, field_len) = read_length(raw, pos, info)?;
            let start = pos + field_len;
            if start % 2 != 0 || start + len * 2 > raw.len() {
                return Err(Error::Invalid("plist unicode string range"));
            }
            let units = raw[start..start + len * 2]
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect::<Vec<_>>();
            let text =
                String::from_utf16(&units).map_err(|_| Error::Invalid("plist unicode string"))?;
            Ok(PlistValue::String(text))
        }
        0x7 => {
            // UID (8 bits of length in low nibble, then bytes).
            let len = info;
            let value = read_be_usize(&raw[pos..], len)?;
            Ok(PlistValue::Data(
                (value as u64).to_be_bytes()[8 - len..].to_vec(),
            ))
        }
        0xa => {
            let (len, field_len) = read_length(raw, pos, info)?;
            let start = pos + field_len;
            if len > MAX_PLIST_OBJECTS {
                return Err(Error::LimitExceeded("plist array size"));
            }
            let mut values = Vec::with_capacity(len);
            for i in 0..len {
                let ref_size = object_ref_size(raw, offsets)?;
                let ref_index = read_object_ref(raw, start + i * ref_size, ref_size)?;
                values.push(parse_plist_object(raw, offsets, ref_index, depth + 1)?);
            }
            Ok(PlistValue::Array(values))
        }
        0xd => {
            let (len, field_len) = read_length(raw, pos, info)?;
            let start = pos + field_len;
            if len > MAX_PLIST_OBJECTS / 2 {
                return Err(Error::LimitExceeded("plist dict size"));
            }
            let ref_size = object_ref_size(raw, offsets)?;
            let value_refs_start = len
                .checked_mul(ref_size)
                .and_then(|size| start.checked_add(size))
                .ok_or(Error::LimitExceeded("plist dict references"))?;
            let mut entries = Vec::with_capacity(len);
            for i in 0..len {
                let key_start = start
                    .checked_add(
                        i.checked_mul(ref_size)
                            .ok_or(Error::LimitExceeded("plist dict references"))?,
                    )
                    .ok_or(Error::LimitExceeded("plist dict references"))?;
                let value_start = value_refs_start
                    .checked_add(
                        i.checked_mul(ref_size)
                            .ok_or(Error::LimitExceeded("plist dict references"))?,
                    )
                    .ok_or(Error::LimitExceeded("plist dict references"))?;
                let key_index = read_object_ref(raw, key_start, ref_size)?;
                let value_index = read_object_ref(raw, value_start, ref_size)?;
                let key = parse_plist_object(raw, offsets, key_index, depth + 1)?;
                let key = match key {
                    PlistValue::String(value) => value,
                    _ => return Err(Error::Invalid("plist dict key")),
                };
                entries.push((
                    key,
                    parse_plist_object(raw, offsets, value_index, depth + 1)?,
                ));
            }
            Ok(PlistValue::Dict(entries))
        }
        0xf => Ok(PlistValue::Null),
        _ => Err(Error::Invalid("plist object type")),
    }
}

fn object_ref_size(raw: &[u8], _offsets: &[usize]) -> Result<usize> {
    // Recover objectRefSize from the trailer.
    const TRAILER_LEN: usize = 32;
    if raw.len() < 8 + TRAILER_LEN {
        return Err(Error::Invalid("plist trailer"));
    }
    Ok(usize::from(raw[raw.len() - TRAILER_LEN + 7]))
}

fn read_object_ref(raw: &[u8], start: usize, ref_size: usize) -> Result<usize> {
    read_be_usize(
        raw.get(start..).ok_or(Error::Invalid("plist ref range"))?,
        ref_size,
    )
}

fn read_length(raw: &[u8], pos: usize, info: usize) -> Result<(usize, usize)> {
    if info < 0x0f {
        return Ok((info, 0));
    }
    // Extended length fields use an integer marker (0x10=1, 0x11=2,
    // 0x12=4, 0x13=8 bytes) followed by the big-endian length.
    let extra = match *raw.get(pos).ok_or(Error::Invalid("plist length"))? {
        0x10 => 1,
        0x11 => 2,
        0x12 => 4,
        0x13 => 8,
        _ => return Err(Error::Invalid("plist length size")),
    };
    let value = read_be_usize(
        raw.get(pos + 1..)
            .ok_or(Error::Invalid("plist length range"))?,
        extra,
    )?;
    Ok((value, 1 + extra))
}

fn read_be_usize(raw: &[u8], len: usize) -> Result<usize> {
    if len == 0 || len > 8 || raw.len() < len {
        return Err(Error::NeedMore {
            needed: len,
            available: raw.len(),
        });
    }
    let mut value = 0usize;
    for &byte in &raw[..len] {
        value = value
            .checked_shl(8)
            .and_then(|v| v.checked_add(usize::from(byte)))
            .ok_or(Error::LimitExceeded("plist integer"))?;
    }
    Ok(value)
}

// ---------------------------------------------------------------------------
// SDP-style field extraction (binary plist values and plain-text fallback)
// ---------------------------------------------------------------------------

fn codec_from_rtpmap(text: &str) -> Option<MediaStreamCodec> {
    let upper = text.to_ascii_uppercase();
    if upper.contains("H264") {
        Some(MediaStreamCodec::H264)
    } else if upper.contains("HEVC") || upper.contains("H265") {
        Some(MediaStreamCodec::Hevc)
    } else {
        None
    }
}

fn payload_type_from_rtpmap(text: &str) -> Option<u8> {
    let digits: String = text.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn apply_fmtp(text: &str, config: &mut VideoCodecConfig) {
    for part in text.split_whitespace() {
        let profile = part
            .strip_prefix("profile-level-id=")
            .or_else(|| part.strip_prefix("profileLevelID="));
        if let Some(value) = profile {
            config.profile_level_id = Some(value.to_owned());
        } else if let Some(value) = part.strip_prefix("packetization-mode=") {
            config.packetization_mode = value.parse().ok();
        }
    }
}

fn is_sdp_text(raw: &[u8]) -> bool {
    raw.windows(2).any(|w| w == b"m=") && raw.contains(&b'\n')
}

fn parse_sdp_text(raw: &[u8], config: &mut VideoCodecConfig) {
    let text = match std::str::from_utf8(raw) {
        Ok(text) => text,
        Err(_) => return,
    };
    for line in text.lines() {
        let line = line.trim();
        if let Some(rtpmap) = line.strip_prefix("a=rtpmap:") {
            config.codec = codec_from_rtpmap(rtpmap);
            config.payload_type = payload_type_from_rtpmap(rtpmap);
        } else if let Some(fmtp) = line.strip_prefix("a=fmtp:") {
            apply_fmtp(fmtp, config);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_binary_plist_dict() {
        // { "CallID": "ABCD" } encoded as bplist00 by hand.
        // Objects: 0: dict [1,2], 1: string "CallID", 2: string "ABCD".
        let mut raw = b"bplist00".to_vec();
        raw.extend_from_slice(&[0xd1, 0x01, 0x02, 0x56, 0x43, 0x61, 0x6c, 0x6c, 0x49, 0x44]);
        raw.extend_from_slice(&[0x54, 0x41, 0x42, 0x43, 0x44]);
        // offset table: object offsets 8, 11, 18 (1-byte), placed at 23
        raw.extend_from_slice(&[0x08, 0x0b, 0x12]);
        // trailer: 6 zero bytes, offsetSize=1, objectRefSize=1, 3 objects, top=0, table=23
        raw.extend_from_slice(&[0; 6]);
        raw.push(1);
        raw.push(1);
        raw.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 3]);
        raw.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
        raw.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 23]);

        let value = parse_property_list(&raw).expect("plist parses");
        let dict = value.as_dict().expect("top-level dict");
        assert_eq!(dict.len(), 1);
        assert_eq!(dict[0].0, "CallID");
        assert_eq!(dict[0].1.as_string(), Some("ABCD"));
    }

    #[test]
    fn extracts_codec_from_rtpmap_string() {
        let mut config = VideoCodecConfig::default();
        apply_fmtp("profile-level-id=42e01f packetization-mode=1", &mut config);
        assert_eq!(config.profile_level_id.as_deref(), Some("42e01f"));
        assert_eq!(config.packetization_mode, Some(1));
        assert_eq!(
            codec_from_rtpmap("100 HEVC/90000"),
            Some(MediaStreamCodec::Hevc)
        );
        assert_eq!(payload_type_from_rtpmap("100 HEVC/90000"), Some(100));
    }

    #[test]
    fn parses_plain_sdp_text() {
        let sdp = b"v=0\r\nm=video 9 RTP/AVP 123\r\na=rtpmap:123 H264/90000\r\na=fmtp:123 profile-level-id=42e01f\r\n";
        let mut config = VideoCodecConfig::default();
        parse_sdp_text(sdp, &mut config);
        assert_eq!(config.codec, Some(MediaStreamCodec::H264));
        assert_eq!(config.payload_type, Some(123));
        assert_eq!(config.profile_level_id.as_deref(), Some("42e01f"));
    }
}
