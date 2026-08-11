//! Negotiation payloads carried inside Apple media stream messages.
//!
//! The offers and answers are property-list blobs produced by
//! `AVCMediaStreamNegotiator` (AVConference.framework). The native builder
//! serializes them with `NSPropertyListBinaryFormat_v1_0` (format `0xc8`),
//! so they are compact binary plists rather than text SDP. The keys mirror
//! the FaceTime/AVConference negotiation vocabulary:
//!
//! * `avcMediaStreamOptionCallID` (`kAVCMediaStreamOptionCallID`) - UUID
//!   string;
//! * `avcMediaStreamOptionRemoteEndpointInfo`
//!   (`kAVCMediaStreamOptionRemoteEndpointInfo`) - the `VCCallInfoBlob`
//!   data;
//! * `avcMediaStreamNegotiatorMode` and
//!   `avcMediaStreamNegotiatorMediaBlob` - the stream mode and compressed
//!   negotiator capabilities.
//!
//! This module implements a small bounded binary-plist reader plus tolerant
//! extraction of the codec-relevant fields. It also accepts plain-text SDP
//! bodies so a future server variant that negotiates with classic SDP keeps
//! working without changes to the callers.

use std::io::{Read, Write};

use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};

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
    /// Native AVC codec type from the answer, when the plist exposes it.
    /// This is retained separately from the RTP payload number because the
    /// two are distinct negotiated fields.
    pub codec_type: Option<u32>,
    pub payload_type: Option<u8>,
    /// Encoding name as it appeared in the negotiated media description. It
    /// is retained even when the name is not one of the codecs supported by
    /// the platform decoder.
    pub encoding_name: Option<String>,
    /// All RTP payload mappings found in the answer. A payload type is only
    /// selected when the answer explicitly names it or supplies one mapping.
    pub payload_mappings: Vec<VideoPayloadConfig>,
    pub profile_level_id: Option<String>,
    pub packetization_mode: Option<u8>,
    /// RTP clock rate from the selected media description. Native AVC video
    /// uses 90 kHz, but keep the answer value instead of baking it into the
    /// decoder path.
    pub rtp_timestamp_rate: Option<u32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub rtp_extension_profile: Option<u16>,
    /// Native AVC's CVO/RTP header-extension identifier, when the answer
    /// exposes it separately from the extension profile.
    pub cvo_extension_id: Option<u8>,
    pub rtp_extensions: Vec<String>,
}

/// Format capability data nested below one native V2 video payload.
///
/// These values describe the supported format rules; they do not select a
/// codec by themselves. Codec selection remains driven by the outer RTP
/// payload mapping (or an explicit answer `rtpmap`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VideoFormatParameters {
    pub parameter_set: Option<u32>,
    pub encode_formats: Option<u32>,
    pub decode_formats: Option<u32>,
    pub video_payload: Option<u32>,
    pub preferred_decode_format: Option<u32>,
    pub encode_decode_features_present: bool,
}

/// One RTP payload mapping from the negotiated media description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoPayloadConfig {
    pub payload_type: u8,
    pub encoding_name: String,
    pub codec: Option<MediaStreamCodec>,
    pub format_parameters: Vec<VideoFormatParameters>,
}

/// Parsed form of a media-stream offer or answer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MediaStreamOffer {
    pub call_id: Option<String>,
    pub remote_endpoint_info: Option<Vec<u8>>,
    pub mode: Option<i64>,
    pub direction: Option<i64>,
    pub media_blob: Option<Vec<u8>>,
    /// SSRC carried by the compressed media configuration. For a server
    /// answer this is the server RTP stream SSRC used by SRTP.
    pub remote_ssrc: Option<u32>,
    pub codec: VideoCodecConfig,
}

impl MediaStreamOffer {
    /// Parse a bounded binary-plist or SDP negotiation blob.
    pub fn parse(raw: &[u8]) -> Result<Self> {
        if raw.len() > MAX_MEDIA_STREAM_MESSAGE {
            return Err(Error::LimitExceeded("negotiation payload"));
        }
        // AVC media-stream messages carry the binary plist behind a four-byte
        // message discriminator. The discriminator is not part of the plist
        // and is present on answers as well as offers.
        let raw = raw
            .strip_prefix(&[0, 0, 0, 0])
            .filter(|payload| payload.starts_with(b"bplist00"))
            .unwrap_or(raw);
        let mut offer = Self::default();
        let mut media_ssrc_candidates = Vec::new();
        match parse_property_list(raw) {
            Ok(plist) => {
                extract_plist_fields(&plist, &mut offer);
                collect_media_blob_ssrcs(&plist, &mut media_ssrc_candidates);
            }
            Err(_error) if is_sdp_text(raw) => parse_sdp_text(raw, &mut offer.codec),
            Err(error) => {
                let plists = match parse_concatenated_binary_plists(raw) {
                    Ok(plists) => plists,
                    Err(_) => return Err(error),
                };
                for plist in plists {
                    extract_plist_fields(&plist, &mut offer);
                    collect_media_blob_ssrcs(&plist, &mut media_ssrc_candidates);
                }
            }
        }
        if let Some(media_blob) = offer.media_blob.as_deref() {
            apply_media_blob_config(media_blob, &mut offer.codec);
        }
        offer.remote_ssrc = offer
            .media_blob
            .as_deref()
            .and_then(parse_media_blob_ssrc)
            // Native Message2 places audio before video1. If the selected
            // media blob is absent or malformed, prefer the last valid
            // candidate rather than accidentally using audio's SSRC.
            .or_else(|| media_ssrc_candidates.into_iter().rev().flatten().next());
        finalize_codec_config(&mut offer.codec);
        Ok(offer)
    }
}

/// Read the media-config protobuf's stream SSRC (field 5, nested field 1).
/// The native answer stores the media configuration as zlib data inside its
/// binary plist. This value is also what `VCMediaStreamConfig remoteSSRC`
/// supplies to the RTP handle before SRTP is configured.
fn parse_media_blob_ssrc(blob: &[u8]) -> Option<u32> {
    parse_media_blob_info(blob)?.remote_ssrc
}

#[derive(Debug, Default)]
struct MediaBlobInfo {
    remote_ssrc: Option<u32>,
    payload_mappings: Vec<VideoPayloadConfig>,
}

const MAX_MEDIA_BLOB_DECOMPRESSED: usize = MAX_MEDIA_STREAM_MESSAGE * 4;

/// Extract the fields that the native RTP setup consumes from the compressed
/// media protobuf. The top-level field 5 is the media stream configuration;
/// its field 3 entries carry the RTP payload number used by the native
/// `VCMediaNegotiationBlobV2VideoPayload` mapping.
fn parse_media_blob_info(blob: &[u8]) -> Option<MediaBlobInfo> {
    let decoder = ZlibDecoder::new(blob);
    let mut payload = Vec::new();
    decoder
        .take((MAX_MEDIA_BLOB_DECOMPRESSED + 1) as u64)
        .read_to_end(&mut payload)
        .ok()?;
    if payload.len() > MAX_MEDIA_BLOB_DECOMPRESSED {
        return None;
    }

    let mut info = MediaBlobInfo::default();
    let mut position = 0;
    while position < payload.len() {
        let tag = read_proto_varint(&payload, &mut position)?;
        let field = tag >> 3;
        match tag & 0x07 {
            0 => {
                let _ = read_proto_varint(&payload, &mut position)?;
            }
            1 => position = position.checked_add(8)?,
            2 => {
                let length = usize::try_from(read_proto_varint(&payload, &mut position)?).ok()?;
                let end = position.checked_add(length)?;
                if end > payload.len() {
                    return None;
                }
                if field == 5 {
                    parse_media_stream_config(&payload[position..end], &mut info)?;
                }
                position = end;
            }
            5 => position = position.checked_add(4)?,
            _ => return None,
        }
        if position > payload.len() {
            return None;
        }
    }
    Some(info)
}

fn parse_media_stream_config(bytes: &[u8], info: &mut MediaBlobInfo) -> Option<()> {
    let mut position = 0;
    while position < bytes.len() {
        let tag = read_proto_varint(bytes, &mut position)?;
        let field = tag >> 3;
        match tag & 0x07 {
            0 => {
                let value = read_proto_varint(bytes, &mut position)?;
                if field == 1 {
                    info.remote_ssrc = info.remote_ssrc.or_else(|| u32::try_from(value).ok());
                }
            }
            1 => position = position.checked_add(8)?,
            2 => {
                let length = usize::try_from(read_proto_varint(bytes, &mut position)?).ok()?;
                let end = position.checked_add(length)?;
                if end > bytes.len() {
                    return None;
                }
                if field == 3 {
                    parse_media_payload_config(&bytes[position..end], info)?;
                }
                position = end;
            }
            5 => position = position.checked_add(4)?,
            _ => return None,
        }
        if position > bytes.len() {
            return None;
        }
    }
    Some(())
}

fn parse_media_payload_config(bytes: &[u8], info: &mut MediaBlobInfo) -> Option<()> {
    let mut position = 0;
    let mut payload_type = None;
    let mut format_parameters = Vec::new();
    while position < bytes.len() {
        let tag = read_proto_varint(bytes, &mut position)?;
        let field = tag >> 3;
        match tag & 0x07 {
            0 => {
                let value = read_proto_varint(bytes, &mut position)?;
                if field == 1 {
                    payload_type = u8::try_from(value).ok();
                }
            }
            1 => position = position.checked_add(8)?,
            2 => {
                let length = usize::try_from(read_proto_varint(bytes, &mut position)?).ok()?;
                let end = position.checked_add(length)?;
                if end > bytes.len() {
                    return None;
                }
                // Field 2 contains repeated V2 capability descriptors. Keep
                // their confirmed scalar fields, but do not replace the
                // outer RTP payload number with a guessed codec selection.
                if field == 2 {
                    let mut parameters = VideoFormatParameters::default();
                    parse_video_format_parameters(&bytes[position..end], &mut parameters)?;
                    format_parameters.push(parameters);
                }
                position = end;
            }
            5 => position = position.checked_add(4)?,
            _ => return None,
        }
        if position > bytes.len() {
            return None;
        }
    }

    let Some(payload_type) = payload_type else {
        return Some(());
    };
    let codec = native_codec_for_rtp_payload(payload_type);
    let encoding_name = codec.map_or_else(String::new, |codec| codec.name().to_owned());
    if !info
        .payload_mappings
        .iter()
        .any(|mapping| mapping.payload_type == payload_type)
    {
        info.payload_mappings.push(VideoPayloadConfig {
            payload_type,
            encoding_name,
            codec,
            format_parameters,
        });
    } else if let Some(mapping) = info
        .payload_mappings
        .iter_mut()
        .find(|mapping| mapping.payload_type == payload_type)
    {
        mapping.format_parameters.extend(format_parameters);
    }
    Some(())
}

fn parse_video_format_parameters(
    bytes: &[u8],
    parameters: &mut VideoFormatParameters,
) -> Option<()> {
    let mut position = 0;
    while position < bytes.len() {
        let tag = read_proto_varint(bytes, &mut position)?;
        let field = tag >> 3;
        match tag & 0x07 {
            0 => {
                let value = u32::try_from(read_proto_varint(bytes, &mut position)?).ok()?;
                match field {
                    1 => parameters.parameter_set = Some(value),
                    2 => parameters.encode_formats = Some(value),
                    3 => parameters.decode_formats = Some(value),
                    4 => parameters.video_payload = Some(value),
                    6 => parameters.preferred_decode_format = Some(value),
                    _ => {}
                }
            }
            1 => position = position.checked_add(8)?,
            2 => {
                let length = usize::try_from(read_proto_varint(bytes, &mut position)?).ok()?;
                let end = position.checked_add(length)?;
                if end > bytes.len() {
                    return None;
                }
                if field == 5 {
                    parameters.encode_decode_features_present = true;
                }
                position = end;
            }
            5 => position = position.checked_add(4)?,
            _ => return None,
        }
        if position > bytes.len() {
            return None;
        }
    }
    Some(())
}

/// Native `rtpPayloadWithPayload:` maps the two AVC video payload enums to
/// RTP 123 (H.264) and RTP 100 (HEVC). Unknown payload numbers remain
/// unknown; they must not be guessed from packet bytes.
fn native_codec_for_rtp_payload(payload_type: u8) -> Option<MediaStreamCodec> {
    match payload_type {
        123 => Some(MediaStreamCodec::H264),
        100 => Some(MediaStreamCodec::Hevc),
        _ => None,
    }
}

fn native_codec_for_codec_type(codec_type: u32) -> Option<(MediaStreamCodec, &'static str)> {
    match codec_type {
        100 => Some((MediaStreamCodec::H264, "H264")),
        101 => Some((MediaStreamCodec::H264, "H264")),
        102 => Some((MediaStreamCodec::Hevc, "HEVC")),
        _ => None,
    }
}

fn apply_media_blob_config(blob: &[u8], config: &mut VideoCodecConfig) {
    let Some(info) = parse_media_blob_info(blob) else {
        return;
    };
    for mapping in info.payload_mappings {
        if let Some(existing) = config
            .payload_mappings
            .iter_mut()
            .find(|existing| existing.payload_type == mapping.payload_type)
        {
            let mut mapping = mapping;
            if mapping.codec.is_none() {
                mapping.codec = existing.codec;
            }
            if mapping.encoding_name.is_empty() {
                mapping.encoding_name = existing.encoding_name.clone();
            }
            if mapping.format_parameters.is_empty() {
                mapping.format_parameters = std::mem::take(&mut existing.format_parameters);
            }
            *existing = mapping;
        } else {
            config.payload_mappings.push(mapping);
        }
    }
    // A capability blob can list multiple formats. Only a single mapping is
    // safe to promote to the selected payload; an ambiguous answer is left
    // for the explicit plist/SDP fields to resolve.
    if config.payload_type.is_none() && config.payload_mappings.len() == 1 {
        config.payload_type = Some(config.payload_mappings[0].payload_type);
    }
}

fn read_proto_varint(bytes: &[u8], position: &mut usize) -> Option<u64> {
    let mut value = 0_u64;
    for shift in (0..70).step_by(7) {
        let byte = *bytes.get(*position)?;
        *position += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

fn extract_plist_fields(value: &PlistValue, offer: &mut MediaStreamOffer) {
    match value {
        PlistValue::Dict(entries) => {
            for (key, value) in entries {
                match key.as_str() {
                    "CallID" | "callID" | "avcMediaStreamOptionCallID" => {
                        offer.call_id = value.as_string().map(str::to_owned)
                    }
                    "RemoteEndpointInfo"
                    | "remoteEndpointInfo"
                    | "avcMediaStreamOptionRemoteEndpointInfo" => {
                        offer.remote_endpoint_info = value.as_data().map(<[u8]>::to_vec)
                    }
                    "mode" | "streamMode" | "mediaStreamMode" | "avcMediaStreamNegotiatorMode" => {
                        offer.mode = value.as_integer()
                    }
                    "direction" | "mediaStreamDirection" => offer.direction = value.as_integer(),
                    "avcMediaStreamNegotiatorMediaBlob" => {
                        offer.media_blob = value.as_data().map(<[u8]>::to_vec)
                    }
                    "rtpmap" | "RTPMap" => {
                        if let Some(text) = plist_text(value) {
                            record_rtpmap(text, &mut offer.codec);
                        }
                    }
                    "fmtp" | "FMTP" => {
                        if let Some(text) = plist_text(value) {
                            apply_fmtp(text, &mut offer.codec);
                        }
                    }
                    "codec" | "codecName" | "encodingName" => {
                        if let Some(text) = plist_text(value) {
                            offer.codec.encoding_name = Some(text.to_owned());
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
                    "profileLevelId" => {
                        offer.codec.profile_level_id = value.as_string().map(str::to_owned);
                    }
                    "codecType" => {
                        offer.codec.codec_type = value
                            .as_integer()
                            .and_then(|value| u32::try_from(value).ok());
                        if offer.codec.codec.is_none()
                            && let Some(codec_type) = offer.codec.codec_type
                            && let Some((codec, encoding_name)) =
                                native_codec_for_codec_type(codec_type)
                        {
                            offer.codec.codec = Some(codec);
                            offer.codec.encoding_name = Some(encoding_name.to_owned());
                        }
                    }
                    "payloadType" | "rtpPayload" | "payload" => {
                        offer.codec.payload_type = value
                            .as_integer()
                            .and_then(|value| u8::try_from(value).ok());
                    }
                    "rtpExtensionProfile" | "extensionProfile" => {
                        offer.codec.rtp_extension_profile = value
                            .as_integer()
                            .and_then(|value| u16::try_from(value).ok());
                    }
                    "cvoExtensionID" | "cvoExtensionId" => {
                        offer.codec.cvo_extension_id = value
                            .as_integer()
                            .and_then(|value| u8::try_from(value).ok());
                    }
                    "extmap" | "RTPHeaderExtension" | "rtpExtension" => {
                        if let Some(text) = plist_text(value) {
                            offer.codec.rtp_extensions.push(text.to_owned());
                        }
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
                    "rtpSampleRate" | "rtpTimestampRate" | "timestampRate" => {
                        offer.codec.rtp_timestamp_rate = value
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

fn collect_media_blob_ssrcs(value: &PlistValue, output: &mut Vec<Option<u32>>) {
    match value {
        PlistValue::Dict(entries) => {
            for (key, value) in entries {
                if key == "avcMediaStreamNegotiatorMediaBlob"
                    && let Some(blob) = value.as_data()
                {
                    output.push(parse_media_blob_ssrc(blob));
                }
                collect_media_blob_ssrcs(value, output);
            }
        }
        PlistValue::Array(values) => {
            for value in values {
                collect_media_blob_ssrcs(value, output);
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
/// 22 08 "2215.5.1" 2a 05 "25G72"` (31 bytes). Field numbers follow
/// `VCCallInfoBlob`; the framework version is part of the native identity
/// sent by Screen Sharing and is required by some server builds.
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
    field_bytes(4, b"2215.5.1", &mut blob); // framework version
    field_bytes(5, build_version.as_bytes(), &mut blob); // OS/build version
    blob
}

/// Build the binary-plist media-stream offer for one stream.
///
/// The dictionary mirrors `AVCMediaStreamNegotiator -createOffer` on the
/// Screen Sharing build used by this crate. The native negotiator emits four
/// keys: endpoint information, mode, a compressed media capability blob, and
/// the call ID. Direction is intentionally omitted because Screen Sharing's
/// viewer configuration does not negotiate it in this path.
pub fn build_media_stream_offer(
    call_id: &str,
    remote_endpoint_info: &[u8],
    mode: i64,
    _direction: i64,
) -> Result<Vec<u8>> {
    build_media_stream_offer_with_ssrc(
        call_id,
        remote_endpoint_info,
        mode,
        _direction,
        media_stream_ssrc(call_id),
    )
}

/// Build an offer using the native RTP-generated local SSRC.
///
/// `AVCMediaStreamNegotiatorSettings` receives this value from
/// `_RTPGenerateSSRC`, so the live client passes that random SSRC in the media
/// blob. The legacy four-argument helper above remains deterministic for
/// callers that only need a standalone offer.
pub fn build_media_stream_offer_with_ssrc(
    call_id: &str,
    remote_endpoint_info: &[u8],
    mode: i64,
    _direction: i64,
    local_ssrc: u32,
) -> Result<Vec<u8>> {
    build_media_stream_offer_with_optional_codec(
        call_id,
        remote_endpoint_info,
        mode,
        local_ssrc,
        None,
    )
}

/// Build a video offer constrained to one negotiated codec.
pub fn build_media_stream_offer_with_ssrc_and_codec(
    call_id: &str,
    remote_endpoint_info: &[u8],
    mode: i64,
    _direction: i64,
    local_ssrc: u32,
    codec: MediaStreamCodec,
) -> Result<Vec<u8>> {
    if mode != 7 {
        return Err(Error::Invalid("codec preference requires a video offer"));
    }
    build_media_stream_offer_with_optional_codec(
        call_id,
        remote_endpoint_info,
        mode,
        local_ssrc,
        Some(codec),
    )
}

fn build_media_stream_offer_with_optional_codec(
    call_id: &str,
    remote_endpoint_info: &[u8],
    mode: i64,
    local_ssrc: u32,
    codec: Option<MediaStreamCodec>,
) -> Result<Vec<u8>> {
    let media_blob = build_media_stream_media_blob(mode, local_ssrc, codec)?;
    let dict = PlistValueBuilder::Dict(vec![
        (
            "avcMediaStreamOptionRemoteEndpointInfo".to_owned(),
            PlistValueBuilder::Data(remote_endpoint_info.to_vec()),
        ),
        (
            "avcMediaStreamNegotiatorMode".to_owned(),
            PlistValueBuilder::Integer(mode),
        ),
        (
            "avcMediaStreamNegotiatorMediaBlob".to_owned(),
            PlistValueBuilder::Data(media_blob),
        ),
        (
            "avcMediaStreamOptionCallID".to_owned(),
            PlistValueBuilder::String(call_id.to_owned()),
        ),
    ]);
    serialize_property_list(&dict)
}

// These are the protobuf payloads emitted by AVCMediaStreamNegotiator for
// Screen Sharing's audio (mode 8) and video (mode 7) streams on macOS 26.6.
// The payload is zlib-compressed before it is put in the binary plist. The
// only per-offer value that must change is the five-byte protobuf varint used
// as the stream SSRC; keeping its width stable preserves the surrounding
// length-delimited fields.
const NATIVE_VIDEO_MEDIA_BLOB: &[u8] = &[
    0x08, 0x01, 0x10, 0x01, 0x2a, 0xf3, 0x01, 0x08, 0xb8, 0x82, 0x9f, 0xad, 0x0e, 0x10, 0x00, 0x1a,
    0x7f, 0x08, 0x7b, 0x12, 0x0a, 0x08, 0x01, 0x10, 0x01, 0x18, 0xc3, 0x87, 0x03, 0x20, 0x00, 0x12,
    0x0a, 0x08, 0x01, 0x10, 0x02, 0x18, 0xc3, 0x87, 0x03, 0x20, 0x00, 0x12, 0x0a, 0x08, 0x01, 0x10,
    0x01, 0x18, 0xc3, 0x87, 0x03, 0x20, 0x00, 0x12, 0x0a, 0x08, 0x01, 0x10, 0x02, 0x18, 0xc3, 0x87,
    0x03, 0x20, 0x00, 0x1a, 0x49, 0x46, 0x4c, 0x53, 0x3b, 0x4d, 0x53, 0x3a, 0x2d, 0x31, 0x3b, 0x4c,
    0x46, 0x3a, 0x2d, 0x31, 0x3b, 0x4c, 0x54, 0x52, 0x3b, 0x43, 0x41, 0x42, 0x41, 0x43, 0x3b, 0x50,
    0x4f, 0x53, 0x3a, 0x30, 0x3b, 0x45, 0x4f, 0x44, 0x3a, 0x31, 0x3b, 0x48, 0x54, 0x53, 0x3a, 0x32,
    0x3b, 0x52, 0x52, 0x3a, 0x33, 0x3b, 0x41, 0x52, 0x3a, 0x31, 0x36, 0x2f, 0x39, 0x2c, 0x35, 0x2f,
    0x38, 0x3b, 0x58, 0x52, 0x3a, 0x31, 0x36, 0x2f, 0x39, 0x2c, 0x35, 0x2f, 0x38, 0x3b, 0x20, 0x01,
    0x1a, 0x5e, 0x08, 0x64, 0x12, 0x0a, 0x08, 0x01, 0x10, 0x01, 0x18, 0xc3, 0x87, 0x03, 0x20, 0x00,
    0x12, 0x0a, 0x08, 0x01, 0x10, 0x02, 0x18, 0xc3, 0x87, 0x03, 0x20, 0x00, 0x1a, 0x40, 0x46, 0x4c,
    0x53, 0x3b, 0x4c, 0x46, 0x3a, 0x2d, 0x31, 0x3b, 0x50, 0x4f, 0x53, 0x3a, 0x35, 0x3b, 0x45, 0x4f,
    0x44, 0x3a, 0x31, 0x3b, 0x48, 0x54, 0x53, 0x3a, 0x32, 0x3b, 0x52, 0x52, 0x3a, 0x33, 0x3b, 0x50,
    0x4f, 0x53, 0x45, 0x3a, 0x34, 0x3b, 0x41, 0x52, 0x3a, 0x31, 0x36, 0x2f, 0x39, 0x2c, 0x35, 0x2f,
    0x38, 0x3b, 0x58, 0x52, 0x3a, 0x31, 0x36, 0x2f, 0x39, 0x2c, 0x35, 0x2f, 0x38, 0x3b, 0x20, 0x0e,
    0x30, 0x04, 0x38, 0x01, 0x40, 0x3f, 0x48, 0x01, 0x60, 0x01, 0x32, 0x0d, 0x56, 0x69, 0x63, 0x65,
    0x72, 0x6f, 0x79, 0x20, 0x31, 0x2e, 0x37, 0x2e, 0x30, 0x40, 0x00, 0x4a, 0x09, 0x08, 0xea, 0x1f,
    0x10, 0x00, 0x18, 0x80, 0x80, 0x01, 0x4a, 0x0b, 0x08, 0x00, 0x10, 0x80, 0xda, 0xc4, 0x09, 0x18,
    0x80, 0x80, 0x06, 0x4a, 0x0a, 0x08, 0x00, 0x10, 0x80, 0xb4, 0x89, 0x13, 0x18, 0x80, 0x60, 0x4a,
    0x0b, 0x08, 0x00, 0x10, 0x80, 0xc2, 0xd7, 0x2f, 0x18, 0x80, 0x80, 0x40, 0x4a, 0x0b, 0x08, 0x00,
    0x10, 0x80, 0x9b, 0xee, 0x02, 0x18, 0x80, 0x80, 0x08, 0x4a, 0x05, 0x08, 0x01, 0x10, 0xab, 0x02,
    0x4a, 0x05, 0x08, 0x10, 0x10, 0x84, 0x20, 0x4a, 0x0b, 0x08, 0x00, 0x10, 0x80, 0x8e, 0xce, 0x1c,
    0x18, 0x80, 0x80, 0x10, 0x4a, 0x05, 0x08, 0x04, 0x10, 0xe4, 0x32, 0x4a, 0x0b, 0x08, 0x00, 0x10,
    0xc0, 0xd1, 0xe1, 0x23, 0x18, 0x80, 0x80, 0x20, 0x68, 0x80, 0xf0, 0x92, 0xa5, 0xc1, 0xf8, 0xf0,
    0x91, 0xee, 0x01, 0x70, 0x02, 0x80, 0x01, 0x00,
];

const NATIVE_AUDIO_MEDIA_BLOB: &[u8] = &[
    0x08, 0x01, 0x10, 0x01, 0x1a, 0x12, 0x08, 0xeb, 0xfa, 0x8b, 0xa3, 0x0c, 0x10, 0x00, 0x18, 0x00,
    0x20, 0xff, 0xbc, 0x01, 0x28, 0x00, 0x30, 0x00, 0x32, 0x0d, 0x56, 0x69, 0x63, 0x65, 0x72, 0x6f,
    0x79, 0x20, 0x31, 0x2e, 0x37, 0x2e, 0x30, 0x40, 0x00, 0x4a, 0x05, 0x08, 0x04, 0x10, 0xe4, 0x32,
    0x4a, 0x09, 0x08, 0xea, 0x1f, 0x10, 0x00, 0x18, 0x80, 0x80, 0x01, 0x4a, 0x0a, 0x08, 0x00, 0x10,
    0x80, 0xb4, 0x89, 0x13, 0x18, 0x80, 0x60, 0x4a, 0x0b, 0x08, 0x00, 0x10, 0x80, 0x8e, 0xce, 0x1c,
    0x18, 0x80, 0x80, 0x10, 0x4a, 0x0b, 0x08, 0x00, 0x10, 0x80, 0xda, 0xc4, 0x09, 0x18, 0x80, 0x80,
    0x06, 0x4a, 0x0b, 0x08, 0x00, 0x10, 0x80, 0xc2, 0xd7, 0x2f, 0x18, 0x80, 0x80, 0x40, 0x4a, 0x05,
    0x08, 0x01, 0x10, 0xab, 0x02, 0x4a, 0x0b, 0x08, 0x00, 0x10, 0x80, 0x9b, 0xee, 0x02, 0x18, 0x80,
    0x80, 0x08, 0x4a, 0x05, 0x08, 0x10, 0x10, 0x84, 0x20, 0x4a, 0x0b, 0x08, 0x00, 0x10, 0xc0, 0xd1,
    0xe1, 0x23, 0x18, 0x80, 0x80, 0x20, 0x68, 0x80, 0xf0, 0xba, 0xa5, 0xc1, 0xf8, 0xf0, 0x91, 0xee,
    0x01, 0x70, 0x02, 0x80, 0x01, 0x00,
];

fn build_media_stream_media_blob(
    mode: i64,
    ssrc: u32,
    codec: Option<MediaStreamCodec>,
) -> Result<Vec<u8>> {
    let template = match mode {
        7 => NATIVE_VIDEO_MEDIA_BLOB,
        8 => NATIVE_AUDIO_MEDIA_BLOB,
        _ => return Err(Error::Invalid("unsupported AVC media stream mode")),
    };
    let mut payload = template.to_vec();
    let offset = if mode == 7 { 8 } else { 7 };
    write_fixed_five_byte_varint(&mut payload, offset, ssrc);
    if let Some(codec) = codec {
        payload = retain_video_codec(&payload, codec)?;
    }

    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder
        .write_all(&payload)
        .map_err(|_| Error::Invalid("AVC media stream blob compression failed"))?;
    encoder
        .finish()
        .map_err(|_| Error::Invalid("AVC media stream blob compression failed"))
}

fn retain_video_codec(payload: &[u8], codec: MediaStreamCodec) -> Result<Vec<u8>> {
    let mut position = 0;
    while position < payload.len() {
        let field_start = position;
        let tag = read_proto_varint(payload, &mut position)
            .ok_or(Error::Invalid("native video protobuf tag"))?;
        let field = tag >> 3;
        let wire_type = tag & 0x07;
        if field == 5 && wire_type == 2 {
            let length_start = position;
            let length = usize::try_from(
                read_proto_varint(payload, &mut position)
                    .ok_or(Error::Invalid("native video protobuf length"))?,
            )
            .map_err(|_| Error::LimitExceeded("native video protobuf length"))?;
            let data_end = position
                .checked_add(length)
                .filter(|end| *end <= payload.len())
                .ok_or(Error::Invalid("native video protobuf field"))?;
            let filtered = retain_media_payload_codec(&payload[position..data_end], codec)?;
            let mut out = Vec::with_capacity(payload.len());
            out.extend_from_slice(&payload[..length_start]);
            push_proto_varint(filtered.len() as u64, &mut out);
            out.extend_from_slice(&filtered);
            out.extend_from_slice(&payload[data_end..]);
            return Ok(out);
        }
        position = skip_proto_value(payload, position, wire_type)
            .ok_or(Error::Invalid("native video protobuf field"))?;
        if position <= field_start {
            return Err(Error::Invalid("native video protobuf progress"));
        }
    }
    Err(Error::Invalid("native video media configuration missing"))
}

fn retain_media_payload_codec(bytes: &[u8], codec: MediaStreamCodec) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut position = 0;
    let mut retained = 0usize;
    while position < bytes.len() {
        let field_start = position;
        let tag = read_proto_varint(bytes, &mut position)
            .ok_or(Error::Invalid("native media payload tag"))?;
        let field = tag >> 3;
        let wire_type = tag & 0x07;
        let mut keep = true;
        let field_end = if field == 3 && wire_type == 2 {
            let length = usize::try_from(
                read_proto_varint(bytes, &mut position)
                    .ok_or(Error::Invalid("native media payload length"))?,
            )
            .map_err(|_| Error::LimitExceeded("native media payload length"))?;
            let end = position
                .checked_add(length)
                .filter(|end| *end <= bytes.len())
                .ok_or(Error::Invalid("native media payload field"))?;
            let mut info = MediaBlobInfo::default();
            parse_media_payload_config(&bytes[position..end], &mut info)
                .ok_or(Error::Invalid("native media payload config"))?;
            keep = info
                .payload_mappings
                .first()
                .is_some_and(|mapping| mapping.codec == Some(codec));
            if keep {
                retained += 1;
            }
            end
        } else {
            skip_proto_value(bytes, position, wire_type)
                .ok_or(Error::Invalid("native media payload field"))?
        };
        if keep {
            out.extend_from_slice(&bytes[field_start..field_end]);
        }
        position = field_end;
    }
    if retained != 1 {
        return Err(Error::Invalid("requested AVC codec capability missing"));
    }
    Ok(out)
}

fn skip_proto_value(bytes: &[u8], mut position: usize, wire_type: u64) -> Option<usize> {
    match wire_type {
        0 => {
            let _ = read_proto_varint(bytes, &mut position)?;
        }
        1 => position = position.checked_add(8)?,
        2 => {
            let length = usize::try_from(read_proto_varint(bytes, &mut position)?).ok()?;
            position = position.checked_add(length)?;
        }
        5 => position = position.checked_add(4)?,
        _ => return None,
    }
    (position <= bytes.len()).then_some(position)
}

fn push_proto_varint(mut value: u64, out: &mut Vec<u8>) {
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

/// Return the local SSRC encoded in the native media capability blob.
pub fn media_stream_ssrc(call_id: &str) -> u32 {
    let mut value = 0_u32;
    let mut digits = 0_u8;
    for byte in call_id.bytes() {
        let Some(nibble) = (match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }) else {
            continue;
        };
        value = (value << 4) | u32::from(nibble);
        digits = digits.saturating_add(1);
        if digits == 8 {
            break;
        }
    }
    if digits < 8 {
        value = call_id.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
            hash.wrapping_mul(0x0100_0193) ^ u32::from(byte)
        });
    }
    // The native field is encoded in five bytes. Bit 28 guarantees that the
    // last byte remains present even when a caller supplies a short/non-UUID
    // call ID.
    value | 0x1000_0000
}

fn write_fixed_five_byte_varint(bytes: &mut [u8], offset: usize, value: u32) {
    for index in 0..4 {
        bytes[offset + index] = (((value >> (index * 7)) & 0x7f) as u8) | 0x80;
    }
    bytes[offset + 4] = (value >> 28) as u8;
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
            let width = if *value >= 0 {
                if *value <= u8::MAX as i64 {
                    1
                } else if *value <= u16::MAX as i64 {
                    2
                } else if *value <= u32::MAX as i64 {
                    4
                } else {
                    8
                }
            } else {
                8
            };
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
    let max_offset = table_offset
        .checked_sub(1)
        .ok_or(Error::Invalid("empty plist object table"))?;
    let offset_size = if max_offset <= u8::MAX as usize {
        1
    } else if max_offset <= u16::MAX as usize {
        2
    } else if max_offset <= u32::MAX as usize {
        4
    } else {
        8
    };
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

/// Parse the compact-plist sequence used by AVC answer messages.
///
/// A `MediaStreamMessage2` answer is not one property list: the framework
/// places the audio and video negotiator dictionaries next to each other,
/// with a small amount of framing/padding between them. The ordinary plist
/// parser quite correctly rejects that framing because its trailer is no
/// longer at the end of the supplied slice. Locate each `bplist00` header and
/// use the offset-table relationship in its trailer to recover the exact
/// bounded slice before parsing it.
fn parse_concatenated_binary_plists(raw: &[u8]) -> Result<Vec<PlistValue>> {
    let mut starts = Vec::new();
    for (index, window) in raw.windows(8).enumerate() {
        if window == b"bplist00" {
            starts.push(index);
        }
    }
    if starts.is_empty() {
        return Err(Error::Invalid("negotiation payload has no binary plist"));
    }

    let mut values = Vec::with_capacity(starts.len());
    for (index, &start) in starts.iter().enumerate() {
        let upper_bound = starts.get(index + 1).copied().unwrap_or(raw.len());
        let Some(end) = find_binary_plist_end(raw, start, upper_bound) else {
            continue;
        };
        let value = parse_property_list(&raw[start..end])?;
        values.push(value);
    }
    if values.is_empty() {
        return Err(Error::Invalid("negotiation binary plist bounds"));
    }
    Ok(values)
}

fn find_binary_plist_end(raw: &[u8], start: usize, upper_bound: usize) -> Option<usize> {
    let minimum_end = start.checked_add(8 + 32)?;
    if upper_bound < minimum_end || upper_bound > raw.len() {
        return None;
    }

    // Search backward so trailing padding after the plist is harmless. The
    // cheap trailer check avoids invoking the recursive parser for arbitrary
    // candidate byte positions.
    for end in (minimum_end..=upper_bound).rev() {
        let candidate = &raw[start..end];
        if !has_exact_binary_plist_trailer(candidate) {
            continue;
        }
        if parse_property_list(candidate).is_ok() {
            return Some(end);
        }
    }
    None
}

fn has_exact_binary_plist_trailer(raw: &[u8]) -> bool {
    const TRAILER_LEN: usize = 32;
    if raw.len() < 8 + TRAILER_LEN || !raw.starts_with(b"bplist00") {
        return false;
    }
    let trailer = &raw[raw.len() - TRAILER_LEN..];
    let offset_size = usize::from(trailer[6]);
    let object_ref_size = usize::from(trailer[7]);
    if offset_size == 0 || offset_size > 8 || object_ref_size == 0 || object_ref_size > 8 {
        return false;
    }
    let Ok(object_count) = read_be_usize(&trailer[8..16], 8) else {
        return false;
    };
    let Ok(top_object) = read_be_usize(&trailer[16..24], 8) else {
        return false;
    };
    let Ok(table_offset) = read_be_usize(&trailer[24..32], 8) else {
        return false;
    };
    if object_count == 0
        || object_count > MAX_PLIST_OBJECTS
        || top_object >= object_count
        || table_offset < 8
    {
        return false;
    }
    let Some(table_len) = object_count.checked_mul(offset_size) else {
        return false;
    };
    table_offset.checked_add(table_len) == Some(raw.len() - TRAILER_LEN)
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
    let normalized = normalize_rtpmap(text);
    let name = normalized
        .split_whitespace()
        .nth(1)
        .unwrap_or(normalized)
        .split('/')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    match name.as_str() {
        // X-H264 is the private payload name emitted by the native AVC
        // negotiator, but its packetization is still RFC 6184 H.264.
        "H264" | "X-H264" => Some(MediaStreamCodec::H264),
        "HEVC" | "H265" | "X-HEVC" => Some(MediaStreamCodec::Hevc),
        _ => None,
    }
}

fn payload_type_from_rtpmap(text: &str) -> Option<u8> {
    normalize_rtpmap(text)
        .split_whitespace()
        .next()
        .and_then(|value| value.parse().ok())
}

fn normalize_rtpmap(text: &str) -> &str {
    let text = text.trim();
    text.strip_prefix("a=rtpmap:")
        .or_else(|| text.strip_prefix("rtpmap:"))
        .unwrap_or(text)
}

fn apply_fmtp(text: &str, config: &mut VideoCodecConfig) {
    for part in text.split([' ', ';']) {
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

fn record_rtpmap(text: &str, config: &mut VideoCodecConfig) {
    let Some(payload_type) = payload_type_from_rtpmap(text) else {
        return;
    };
    let encoding_name = normalize_rtpmap(text)
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.split('/').next())
        .unwrap_or_default()
        .to_owned();
    let timestamp_rate = normalize_rtpmap(text)
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.split('/').nth(1))
        .and_then(|value| value.parse::<u32>().ok());
    if timestamp_rate.is_some() {
        config.rtp_timestamp_rate = timestamp_rate;
    }
    let mapping = VideoPayloadConfig {
        payload_type,
        encoding_name: encoding_name.clone(),
        codec: codec_from_rtpmap(text),
        format_parameters: Vec::new(),
    };
    if let Some(existing) = config
        .payload_mappings
        .iter_mut()
        .find(|mapping| mapping.payload_type == payload_type)
    {
        let mut mapping = mapping;
        if mapping.codec.is_none() {
            mapping.codec = existing.codec;
        }
        if mapping.encoding_name.is_empty() {
            mapping.encoding_name = existing.encoding_name.clone();
        }
        if mapping.format_parameters.is_empty() {
            mapping.format_parameters = std::mem::take(&mut existing.format_parameters);
        }
        *existing = mapping;
    } else {
        config.payload_mappings.push(mapping);
    }
}

fn finalize_codec_config(config: &mut VideoCodecConfig) {
    let Some(payload_type) = config.payload_type.or_else(|| {
        (config.payload_mappings.len() == 1).then(|| config.payload_mappings[0].payload_type)
    }) else {
        return;
    };
    config.payload_type = Some(payload_type);
    if let Some(mapping) = config
        .payload_mappings
        .iter()
        .find(|mapping| mapping.payload_type == payload_type)
    {
        if mapping.codec.is_some() {
            config.codec = mapping.codec;
        }
        if !mapping.encoding_name.is_empty() {
            config.encoding_name = Some(mapping.encoding_name.clone());
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
        if let Some(media) = line.strip_prefix("m=video ") {
            if let Some(payload) = media
                .split_whitespace()
                .nth(2)
                .and_then(|value| value.parse().ok())
            {
                config.payload_type = Some(payload);
            }
        } else if let Some(rtpmap) = line.strip_prefix("a=rtpmap:") {
            record_rtpmap(rtpmap, config);
        } else if let Some(fmtp) = line.strip_prefix("a=fmtp:") {
            apply_fmtp(fmtp, config);
        } else if let Some(extmap) = line.strip_prefix("a=extmap:") {
            config.rtp_extensions.push(extmap.to_owned());
        }
    }
    finalize_codec_config(config);
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
    fn parses_answer_with_concatenated_binary_plists() {
        let first = serialize_property_list(&PlistValueBuilder::Dict(vec![(
            "avcMediaStreamOptionCallID".to_owned(),
            PlistValueBuilder::String("call-1".to_owned()),
        )]))
        .expect("first plist");
        let second = serialize_property_list(&PlistValueBuilder::Dict(vec![(
            "codecName".to_owned(),
            PlistValueBuilder::String("H264".to_owned()),
        )]))
        .expect("second plist");

        let mut answer = vec![0, 0, 0, 0];
        answer.extend_from_slice(&first);
        answer.extend_from_slice(&[0xaa; 13]);
        answer.extend_from_slice(&second);

        let parsed = MediaStreamOffer::parse(&answer).expect("answer parses");
        assert_eq!(parsed.call_id.as_deref(), Some("call-1"));
        assert_eq!(parsed.codec.codec, Some(MediaStreamCodec::H264));
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
        assert_eq!(
            codec_from_rtpmap("rtpmap:126 X-H264/90000"),
            Some(MediaStreamCodec::H264)
        );
        assert_eq!(
            payload_type_from_rtpmap("a=rtpmap:126 X-H264/90000"),
            Some(126)
        );
    }

    #[test]
    fn parses_plain_sdp_text() {
        let sdp = b"v=0\r\nm=video 9 RTP/AVP 123\r\na=rtpmap:123 H264/90000\r\na=fmtp:123 profile-level-id=42e01f\r\n";
        let mut config = VideoCodecConfig::default();
        parse_sdp_text(sdp, &mut config);
        assert_eq!(config.codec, Some(MediaStreamCodec::H264));
        assert_eq!(config.payload_type, Some(123));
        assert_eq!(config.rtp_timestamp_rate, Some(90_000));
        assert_eq!(config.profile_level_id.as_deref(), Some("42e01f"));
    }

    #[test]
    fn selects_codec_from_the_answer_media_payload_order() {
        let sdp = b"v=0\r\nm=video 9 RTP/AVP 100 123\r\na=rtpmap:123 H264/90000\r\na=rtpmap:100 HEVC/90000\r\n";
        let mut config = VideoCodecConfig::default();
        parse_sdp_text(sdp, &mut config);
        assert_eq!(config.payload_type, Some(100));
        assert_eq!(config.codec, Some(MediaStreamCodec::Hevc));
        assert_eq!(config.encoding_name.as_deref(), Some("HEVC"));
        assert_eq!(config.payload_mappings.len(), 2);
    }

    #[test]
    fn preserves_private_encoding_without_selecting_a_public_decoder() {
        let sdp = b"v=0\r\nm=video 9 RTP/AVP 100\r\na=rtpmap:100 X-PRIVATE/90000\r\n";
        let mut config = VideoCodecConfig::default();
        parse_sdp_text(sdp, &mut config);
        assert_eq!(config.payload_type, Some(100));
        assert_eq!(config.codec, None);
        assert_eq!(config.encoding_name.as_deref(), Some("X-PRIVATE"));
    }

    #[test]
    fn retains_explicit_native_video_selection_fields() {
        let raw = serialize_property_list(&PlistValueBuilder::Dict(vec![
            ("codecType".to_owned(), PlistValueBuilder::Integer(102)),
            ("rtpPayload".to_owned(), PlistValueBuilder::Integer(100)),
            (
                "rtpSampleRate".to_owned(),
                PlistValueBuilder::Integer(90_000),
            ),
            ("cvoExtensionID".to_owned(), PlistValueBuilder::Integer(1)),
        ]))
        .expect("plist");
        let parsed = MediaStreamOffer::parse(&raw).expect("parse answer");
        assert_eq!(parsed.codec.codec_type, Some(102));
        assert_eq!(parsed.codec.codec, Some(MediaStreamCodec::Hevc));
        assert_eq!(parsed.codec.payload_type, Some(100));
        assert_eq!(parsed.codec.rtp_timestamp_rate, Some(90_000));
        assert_eq!(parsed.codec.cvo_extension_id, Some(1));
    }

    #[test]
    fn extracts_video_ssrc_from_compressed_media_configuration() {
        let offer = build_media_stream_offer_with_ssrc(
            "00112233-4455-6677-8899-aabbccddeeff",
            &build_remote_endpoint_info("Mac16,12", "25G72"),
            7,
            2,
            0x8123_4567,
        )
        .expect("offer");
        let parsed = MediaStreamOffer::parse(&offer).expect("parse offer");
        assert_eq!(parsed.remote_ssrc, Some(0x8123_4567));
        assert_eq!(parsed.codec.payload_mappings.len(), 2);
        assert_eq!(parsed.codec.payload_type, None);
        assert_eq!(parsed.codec.codec, None);
        assert_eq!(
            parsed
                .codec
                .payload_mappings
                .iter()
                .map(|mapping| mapping.payload_type)
                .collect::<Vec<_>>(),
            vec![123, 100]
        );
    }

    #[test]
    fn constrains_native_video_offer_to_requested_codec() {
        let endpoint = build_remote_endpoint_info("Mac16,12", "25G72");
        for (codec, payload_type) in [(MediaStreamCodec::Hevc, 100), (MediaStreamCodec::H264, 123)]
        {
            let offer = build_media_stream_offer_with_ssrc_and_codec(
                "00112233-4455-6677-8899-aabbccddeeff",
                &endpoint,
                7,
                2,
                0x8123_4567,
                codec,
            )
            .expect("constrained offer");
            let parsed = MediaStreamOffer::parse(&offer).expect("parse constrained offer");

            assert_eq!(parsed.remote_ssrc, Some(0x8123_4567));
            assert_eq!(parsed.codec.codec, Some(codec));
            assert_eq!(parsed.codec.payload_type, Some(payload_type));
            assert_eq!(parsed.codec.payload_mappings.len(), 1);
            assert_eq!(parsed.codec.payload_mappings[0].codec, Some(codec));
            assert_eq!(parsed.codec.payload_mappings[0].payload_type, payload_type);
        }
    }

    #[test]
    fn selects_codec_from_a_single_native_media_payload() {
        let compressed = {
            // field 5: stream config { ssrc, field 3: payload { PT 100,
            // V2 payload enum 2 (HEVC) } }.
            let protobuf = [
                0x2a, 0x0d, 0x08, 0xb4, 0x24, 0x1a, 0x08, 0x08, 0x64, 0x12, 0x04, 0x08, 0x02, 0x20,
                0x00,
            ];
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
            encoder.write_all(&protobuf).expect("compress media blob");
            encoder.finish().expect("finish media blob")
        };
        let raw = serialize_property_list(&PlistValueBuilder::Dict(vec![(
            "avcMediaStreamNegotiatorMediaBlob".to_owned(),
            PlistValueBuilder::Data(compressed),
        )]))
        .expect("plist");

        let parsed = MediaStreamOffer::parse(&raw).expect("parse answer");
        assert_eq!(parsed.remote_ssrc, Some(0x1234));
        assert_eq!(parsed.codec.payload_type, Some(100));
        assert_eq!(parsed.codec.codec, Some(MediaStreamCodec::Hevc));
        assert_eq!(parsed.codec.encoding_name.as_deref(), Some("HEVC"));
        assert_eq!(
            parsed.codec.payload_mappings[0].format_parameters[0].parameter_set,
            Some(2)
        );
        assert_eq!(
            parsed.codec.payload_mappings[0].format_parameters[0].video_payload,
            Some(0)
        );
    }

    #[test]
    fn selects_video_ssrc_from_audio_then_video_answers() {
        let endpoint = build_remote_endpoint_info("Mac16,12", "25G72");
        let audio = build_media_stream_offer_with_ssrc(
            "00112233-4455-6677-8899-aabbccddeeff",
            &endpoint,
            8,
            1,
            0x1111_2222,
        )
        .expect("audio answer");
        let video = build_media_stream_offer_with_ssrc(
            "00112233-4455-6677-8899-aabbccddeeff",
            &endpoint,
            7,
            2,
            0x3333_4444,
        )
        .expect("video answer");
        let mut answer = audio;
        answer.extend_from_slice(&[0xaa; 7]);
        answer.extend_from_slice(&video);

        let parsed = MediaStreamOffer::parse(&answer).expect("parse answers");
        assert_eq!(parsed.mode, Some(7));
        assert_eq!(parsed.remote_ssrc, Some(0x3333_4444));
    }
}
