//! RTP receive path for the Apple media stream.
//!
//! The server pushes H.264 (payload types 123/126) or HEVC (payload type
//! 100) over UDP as RFC 3550 RTP packets. This module parses the RTP header,
//! strips SRTP (see [`crate::media_stream::srtp`]), reassembles fragmented NAL units
//! (RFC 6184 FU-A/STAP-A/STAP-B for H.264, RFC 7798 FU/AP for HEVC) and emits whole
//! access units for the platform decoder.

use crate::{Error, Result};

use super::negotiation::MediaStreamCodec;
use super::{MAX_NAL_UNITS_PER_ACCESS_UNIT, MAX_RTP_PACKET};

const MAX_ASSEMBLED_NAL_BYTES: usize = 16 * 1024 * 1024;
/// Bound the amount of authenticated media held while waiting for a small
/// UDP reordering burst. SRTP replay state is updated on arrival; this queue
/// only changes the order in which clear RTP reaches the depacketizer.
const MAX_RTP_REORDER_PACKETS: usize = 64;

/// Parsed RTP header (fixed 12-byte part plus CSRC list).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpHeader {
    pub version: u8,
    pub padding: bool,
    pub extension: bool,
    pub marker: bool,
    pub payload_type: u8,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
}

/// A parsed RTP packet borrowed from a datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpPacket<'a> {
    pub header: RtpHeader,
    pub payload: &'a [u8],
    /// RFC 3550 header-extension profile, when the extension bit is set.
    pub extension_profile: Option<u16>,
    /// Extension bytes, excluding the four-byte profile/length prefix.
    pub extension_data: &'a [u8],
    /// Offset of `payload` in the borrowed datagram.
    pub payload_offset: usize,
    /// End of the payload before RTP padding is removed.
    pub payload_end: usize,
    /// Number of padding bytes removed by [`Self::parse`].
    pub padding_len: usize,
    /// Total bytes consumed from the datagram (header + extensions + payload).
    pub wire_len: usize,
}

impl<'a> RtpPacket<'a> {
    /// Parse an RTP datagram after its encrypted payload has been decrypted.
    pub fn parse(datagram: &'a [u8]) -> Result<Self> {
        Self::parse_internal(datagram, true)
    }

    /// Parse an RTP body before SRTP decryption. The clear header and
    /// extension lengths are validated, but the final RTP padding byte is
    /// still encrypted and therefore cannot be inspected yet.
    pub fn parse_encrypted(datagram: &'a [u8]) -> Result<Self> {
        Self::parse_internal(datagram, false)
    }

    fn parse_internal(datagram: &'a [u8], validate_padding: bool) -> Result<Self> {
        if datagram.len() < 12 {
            return Err(Error::NeedMore {
                needed: 12,
                available: datagram.len(),
            });
        }
        if datagram.len() > MAX_RTP_PACKET {
            return Err(Error::LimitExceeded("RTP packet"));
        }
        let version = datagram[0] >> 6;
        if version != 2 {
            return Err(Error::Invalid("RTP version"));
        }
        let padding = datagram[0] & 0x20 != 0;
        let extension = datagram[0] & 0x10 != 0;
        let csrc_count = usize::from(datagram[0] & 0x0f);
        let marker = datagram[1] & 0x80 != 0;
        let payload_type = datagram[1] & 0x7f;
        let sequence = u16::from_be_bytes([datagram[2], datagram[3]]);
        let timestamp = u32::from_be_bytes(datagram[4..8].try_into().expect("slice"));
        let ssrc = u32::from_be_bytes(datagram[8..12].try_into().expect("slice"));

        let csrc_bytes = csrc_count
            .checked_mul(4)
            .ok_or(Error::LimitExceeded("RTP CSRC list"))?;
        let mut offset = 12usize
            .checked_add(csrc_bytes)
            .ok_or(Error::LimitExceeded("RTP header"))?;
        if offset > datagram.len() {
            return Err(Error::NeedMore {
                needed: offset,
                available: datagram.len(),
            });
        }
        let mut extension_profile = None;
        let mut extension_data = &datagram[offset..offset];
        if extension {
            // RFC 3550 header extension: u16 profile, u16 length (in 32-bit words).
            let extension_header_end = offset
                .checked_add(4)
                .ok_or(Error::LimitExceeded("RTP extension"))?;
            if extension_header_end > datagram.len() {
                return Err(Error::NeedMore {
                    needed: extension_header_end,
                    available: datagram.len(),
                });
            }
            extension_profile = Some(u16::from_be_bytes([datagram[offset], datagram[offset + 1]]));
            let ext_len = usize::from(u16::from_be_bytes([
                datagram[offset + 2],
                datagram[offset + 3],
            ])) * 4;
            let extension_end = extension_header_end
                .checked_add(ext_len)
                .ok_or(Error::LimitExceeded("RTP extension"))?;
            if extension_end > datagram.len() {
                return Err(Error::NeedMore {
                    needed: extension_end,
                    available: datagram.len(),
                });
            }
            extension_data = &datagram[extension_header_end..extension_end];
            offset = extension_end;
        }
        let padding_len = if padding && validate_padding {
            if datagram.len() <= offset {
                return Err(Error::NeedMore {
                    needed: offset + 1,
                    available: datagram.len(),
                });
            }
            let pad_len = usize::from(datagram[datagram.len() - 1]);
            if pad_len == 0 || pad_len > datagram.len().saturating_sub(offset) {
                return Err(Error::Invalid("RTP padding length"));
            }
            pad_len
        } else {
            0
        };
        let payload_end = datagram
            .len()
            .checked_sub(padding_len)
            .ok_or(Error::Invalid("RTP padding length"))?;
        if offset > payload_end {
            return Err(Error::Invalid("RTP payload range"));
        }
        Ok(Self {
            header: RtpHeader {
                version,
                padding,
                extension,
                marker,
                payload_type,
                sequence,
                timestamp,
                ssrc,
            },
            payload: &datagram[offset..payload_end],
            extension_profile,
            extension_data,
            payload_offset: offset,
            payload_end,
            padding_len,
            wire_len: payload_end,
        })
    }
}

/// Bounded RTP reordering for depacketizers that require contiguous FU
/// sequence numbers. Packets are authenticated and decrypted before entering
/// this queue. A marker packet flushes only after the queued burst is
/// contiguous and begins with a complete NAL or a fragment start; this keeps
/// an end fragment that arrived first from being handed to the depacketizer.
/// A full queue still flushes the current burst so loss is handled by the
/// depacketizer rather than by fabricating packets.
pub(crate) struct RtpReorderBuffer {
    packets: Vec<Vec<u8>>,
    last_released_sequence: Option<u16>,
    /// Last released packet when the current access unit crossed the bounded
    /// reorder window. The following burst may legitimately begin with a FU
    /// continuation rather than a new NAL start.
    continuation: Option<(u16, u32)>,
    codec: MediaStreamCodec,
    marker_pending: bool,
    dropped_access_units: usize,
}

impl RtpReorderBuffer {
    pub(crate) fn with_codec(codec: MediaStreamCodec) -> Self {
        Self {
            packets: Vec::with_capacity(MAX_RTP_REORDER_PACKETS),
            last_released_sequence: None,
            continuation: None,
            codec,
            marker_pending: false,
            dropped_access_units: 0,
        }
    }

    pub(crate) fn push(&mut self, packet: &[u8]) -> Result<Vec<Vec<u8>>> {
        let parsed = RtpPacket::parse(packet)?;
        if let Some(last_sequence) = self.last_released_sequence
            && !sequence_is_newer(parsed.header.sequence, last_sequence)
        {
            // SRTP has already authenticated this packet, but it arrived
            // after the burst containing its sequence had been released. Do
            // not let a late fragment become the beginning of the next frame.
            return Ok(Vec::new());
        }
        if self.packets.iter().any(|queued| {
            RtpPacket::parse(queued)
                .expect("reorder buffer stores parsed RTP packets")
                .header
                .sequence
                == parsed.header.sequence
        }) {
            return Ok(Vec::new());
        }
        self.drop_stale_access_units_before(&parsed);
        self.packets.push(packet.to_vec());
        self.marker_pending |= parsed.header.marker;
        if self.packets.len() >= MAX_RTP_REORDER_PACKETS
            || (self.marker_pending && self.marker_ready())
        {
            return Ok(self.drain_sorted());
        }
        Ok(Vec::new())
    }

    /// Number of access units that were conclusively missing one or more RTP
    /// packets. The caller must reset prediction/depacketization state before
    /// consuming later frames.
    pub(crate) fn take_dropped_access_units(&mut self) -> usize {
        std::mem::take(&mut self.dropped_access_units)
    }

    /// Discard authenticated packets that belong to the prediction chain
    /// being reset. Preserve the released sequence watermark so late UDP
    /// fragments from that chain cannot seed the next codec sync frame.
    pub(crate) fn reset_pending(&mut self) {
        self.packets.clear();
        self.continuation = None;
        self.marker_pending = false;
        self.dropped_access_units = 0;
    }

    fn drop_stale_access_units_before(&mut self, incoming: &RtpPacket<'_>) {
        // Wait until the marker of a newer frame. This gives a late packet an
        // entire frame interval to close the older burst while keeping loss
        // recovery bounded to one frame instead of the 64-packet hard cap.
        if !incoming.header.marker {
            return;
        }
        let stale_headers: Vec<_> = self
            .packets
            .iter()
            .filter_map(|packet| RtpPacket::parse(packet).ok().map(|packet| packet.header))
            .filter(|header| header.timestamp != incoming.header.timestamp)
            .collect();
        let Some(last_stale) = stale_headers
            .iter()
            .copied()
            .max_by(|left, right| sequence_order(left.sequence, right.sequence))
        else {
            return;
        };
        if !sequence_is_newer(incoming.header.sequence, last_stale.sequence) {
            return;
        }

        // A complete newer access unit has arrived, but an older timestamp is
        // still buffered. Discard every stale access unit instead of handing
        // an incomplete predictive picture to the decoder.
        self.packets.retain(|packet| {
            RtpPacket::parse(packet)
                .map(|packet| packet.header.timestamp == incoming.header.timestamp)
                .unwrap_or(false)
        });
        self.last_released_sequence = Some(last_stale.sequence);
        self.continuation = None;
        self.marker_pending = self.packets.iter().any(|packet| {
            RtpPacket::parse(packet)
                .map(|packet| packet.header.marker)
                .unwrap_or(false)
        });
        let mut stale_timestamps = stale_headers
            .iter()
            .map(|header| header.timestamp)
            .collect::<Vec<_>>();
        stale_timestamps.sort_unstable();
        stale_timestamps.dedup();
        self.dropped_access_units = self
            .dropped_access_units
            .saturating_add(stale_timestamps.len());
    }

    fn marker_ready(&mut self) -> bool {
        self.packets.sort_by(|left, right| {
            let left_sequence = RtpPacket::parse(left)
                .expect("reorder buffer stores parsed RTP packets")
                .header
                .sequence;
            let right_sequence = RtpPacket::parse(right)
                .expect("reorder buffer stores parsed RTP packets")
                .header
                .sequence;
            sequence_order(left_sequence, right_sequence)
        });
        let Some(first) = self
            .packets
            .first()
            .and_then(|packet| RtpPacket::parse(packet).ok())
        else {
            return false;
        };
        let Some(marker_timestamp) = self
            .packets
            .iter()
            .filter_map(|packet| RtpPacket::parse(packet).ok())
            .find(|packet| packet.header.marker)
            .map(|packet| packet.header.timestamp)
        else {
            return false;
        };
        let resumes_released_fragment = self.continuation.is_some_and(|(sequence, timestamp)| {
            first.header.sequence == sequence.wrapping_add(1) && first.header.timestamp == timestamp
        });
        if first.header.timestamp != marker_timestamp
            || (!resumes_released_fragment && !packet_can_start_nal(&first, self.codec))
        {
            return false;
        }
        self.packets.windows(2).all(|packets| {
            let left = RtpPacket::parse(&packets[0])
                .expect("reorder buffer stores parsed RTP packets")
                .header
                .sequence;
            let right = RtpPacket::parse(&packets[1])
                .expect("reorder buffer stores parsed RTP packets")
                .header
                .sequence;
            left.wrapping_add(1) == right
        })
    }

    fn drain_sorted(&mut self) -> Vec<Vec<u8>> {
        self.packets.sort_by(|left, right| {
            let left_sequence = RtpPacket::parse(left)
                .expect("reorder buffer stores parsed RTP packets")
                .header
                .sequence;
            let right_sequence = RtpPacket::parse(right)
                .expect("reorder buffer stores parsed RTP packets")
                .header
                .sequence;
            sequence_order(left_sequence, right_sequence)
        });
        let last = self
            .packets
            .last()
            .and_then(|packet| RtpPacket::parse(packet).ok())
            .map(|packet| packet.header);
        self.last_released_sequence = last.map(|header| header.sequence);
        self.continuation =
            last.and_then(|header| (!header.marker).then_some((header.sequence, header.timestamp)));
        self.marker_pending = false;
        std::mem::take(&mut self.packets)
    }
}

fn packet_can_start_nal(packet: &RtpPacket<'_>, codec: MediaStreamCodec) -> bool {
    match codec {
        MediaStreamCodec::H264 => match packet.payload.first().map(|byte| byte & 0x1f) {
            Some(28 | 29) => packet.payload.get(1).is_some_and(|byte| byte & 0x80 != 0),
            Some(_) => true,
            None => false,
        },
        MediaStreamCodec::Hevc => match packet.payload.first().map(|byte| (byte >> 1) & 0x3f) {
            Some(49) => packet.payload.get(2).is_some_and(|byte| byte & 0x80 != 0),
            Some(_) => true,
            None => false,
        },
    }
}

fn sequence_is_newer(candidate: u16, reference: u16) -> bool {
    let distance = candidate.wrapping_sub(reference);
    distance != 0 && distance < 0x8000
}

fn sequence_order(left: u16, right: u16) -> std::cmp::Ordering {
    if left == right {
        std::cmp::Ordering::Equal
    } else if (left.wrapping_sub(right) as i16) < 0 {
        std::cmp::Ordering::Less
    } else {
        std::cmp::Ordering::Greater
    }
}

/// One decoded NAL unit (without start code or length prefix).
pub type NalUnit = Vec<u8>;

/// A complete access unit ready for the decoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessUnit {
    pub timestamp: u32,
    /// Low 16 bits of the codec decoding-order number carried by Apple's
    /// HEVC DONL or H.264 DON field. The four SSRCs share this sequence, so it
    /// is the authoritative cross-stream decode order.
    pub decode_order_number: Option<u16>,
    pub nal_units: Vec<NalUnit>,
}

impl AccessUnit {
    /// Serialize as Annex-B byte stream (00 00 00 01 start codes).
    pub fn to_annex_b(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for nal in &self.nal_units {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(nal);
        }
        out
    }

    /// Serialize with 4-byte big-endian length prefixes (AVCC format).
    pub fn to_avcc(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for nal in &self.nal_units {
            out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            out.extend_from_slice(nal);
        }
        out
    }

    /// Length of the AVCC representation without allocating it.
    pub fn avcc_len(&self) -> usize {
        self.nal_units
            .iter()
            .fold(0usize, |total, nal| total.saturating_add(4 + nal.len()))
    }

    pub fn is_idr(&self) -> bool {
        self.is_sync(MediaStreamCodec::H264)
    }

    /// Whether this access unit can start a fresh codec prediction chain.
    /// H.264 IDR pictures and HEVC IRAP pictures are the only native AVC
    /// recovery boundaries accepted here; predictive pictures must never be
    /// submitted after packet loss or a decoder reset.
    pub fn is_sync(&self, codec: MediaStreamCodec) -> bool {
        self.nal_units.iter().any(|nal| {
            nal.first().is_some_and(|&first| match codec {
                MediaStreamCodec::H264 => first & 0x1f == 5,
                MediaStreamCodec::Hevc => (16..=23).contains(&((first >> 1) & 0x3f)),
            })
        })
    }
}

/// Shared de-fragmentation state.
struct FragmentBuffer {
    bytes: Vec<u8>,
    nal_header: Vec<u8>,
    decode_order_number: Option<u16>,
    timestamp: u32,
    expected_next: u16,
}

impl FragmentBuffer {
    fn new(
        sequence: u16,
        timestamp: u32,
        decode_order_number: Option<u16>,
        nal_header: Vec<u8>,
        first_fragment: &[u8],
    ) -> Self {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(first_fragment);
        Self {
            bytes,
            nal_header,
            decode_order_number,
            timestamp,
            expected_next: sequence.wrapping_add(1),
        }
    }

    fn append(&mut self, sequence: u16, timestamp: u32, fragment: &[u8]) -> bool {
        if timestamp != self.timestamp
            || sequence != self.expected_next
            || self.bytes.len().saturating_add(fragment.len()) > MAX_ASSEMBLED_NAL_BYTES
        {
            return false;
        }
        self.bytes.extend_from_slice(fragment);
        self.expected_next = sequence.wrapping_add(1);
        true
    }

    fn finish(&self) -> NalUnit {
        let mut nal = self.nal_header.clone();
        nal.extend_from_slice(&self.bytes);
        nal
    }
}

/// Frame assembler shared by the H.264/HEVC depacketizers.
///
/// NAL units with the same RTP timestamp accumulate into one access unit;
/// the unit is emitted when the RTP marker bit is set (end of frame) or when
/// a new timestamp begins.
struct FrameAssembler {
    current_timestamp: Option<u32>,
    current_decode_order_number: Option<u16>,
    nal_units: Vec<NalUnit>,
    bytes: usize,
}

impl Default for FrameAssembler {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameAssembler {
    fn new() -> Self {
        Self {
            current_timestamp: None,
            current_decode_order_number: None,
            nal_units: Vec::new(),
            bytes: 0,
        }
    }

    fn push(
        &mut self,
        timestamp: u32,
        decode_order_number: Option<u16>,
        units: Vec<NalUnit>,
        marker: bool,
    ) -> Result<Option<AccessUnit>> {
        if units.is_empty() {
            return Ok(None);
        }
        if units.len() > MAX_NAL_UNITS_PER_ACCESS_UNIT {
            return Err(Error::LimitExceeded("RTP access-unit NAL count"));
        }
        let incoming_bytes = units.iter().try_fold(0usize, |total, unit| {
            total
                .checked_add(unit.len())
                .filter(|size| *size <= MAX_ASSEMBLED_NAL_BYTES)
                .ok_or(Error::LimitExceeded("RTP access-unit size"))
        })?;
        if let Some(previous_ts) = self.current_timestamp {
            if previous_ts != timestamp {
                let previous = std::mem::take(&mut self.nal_units);
                let previous_decode_order_number = self.current_decode_order_number.take();
                self.bytes = incoming_bytes;
                self.current_timestamp = Some(timestamp);
                self.current_decode_order_number = decode_order_number;
                self.nal_units = units;
                return Ok(Some(AccessUnit {
                    timestamp: previous_ts,
                    decode_order_number: previous_decode_order_number,
                    nal_units: previous,
                }));
            }
        } else {
            self.current_timestamp = Some(timestamp);
        }
        if decode_order_number.is_some() {
            self.current_decode_order_number = decode_order_number;
        }
        self.bytes = self
            .bytes
            .checked_add(incoming_bytes)
            .filter(|size| *size <= MAX_ASSEMBLED_NAL_BYTES)
            .ok_or(Error::LimitExceeded("RTP access-unit size"))?;
        self.nal_units.extend(units);
        if marker {
            let nal_units = std::mem::take(&mut self.nal_units);
            let decode_order_number = self.current_decode_order_number.take();
            self.current_timestamp = None;
            self.bytes = 0;
            Ok(Some(AccessUnit {
                timestamp,
                decode_order_number,
                nal_units,
            }))
        } else {
            Ok(None)
        }
    }

    /// Flush any buffered NAL units (used when a fragment is lost).
    fn reset(&mut self) {
        self.current_timestamp = None;
        self.current_decode_order_number = None;
        self.nal_units.clear();
        self.bytes = 0;
    }
}

/// H.264 RTP depacketizer (RFC 6184).
pub struct H264Depacketizer {
    fragment: Option<FragmentBuffer>,
    assembler: FrameAssembler,
}

impl Default for H264Depacketizer {
    fn default() -> Self {
        Self::new()
    }
}

impl H264Depacketizer {
    pub fn new() -> Self {
        Self {
            fragment: None,
            assembler: FrameAssembler::new(),
        }
    }

    pub(crate) fn reset(&mut self) {
        self.fragment = None;
        self.assembler.reset();
    }

    /// Feed one parsed RTP packet whose payload is a raw H.264 payload.
    /// Returns the completed access unit, if any.
    pub fn push(&mut self, packet: &RtpPacket<'_>) -> Result<Option<AccessUnit>> {
        let payload = packet.payload;
        if payload.is_empty() {
            return Ok(None);
        }
        let nal_type = payload[0] & 0x1f;
        let marker = packet.header.marker;
        let ts = packet.header.timestamp;
        if std::env::var_os("ARD_RTP_WIRE_TRACE").is_some() {
            let prefix = &payload[..payload.len().min(9)];
            eprintln!(
                "H264 wire: seq={} ts={ts} marker={marker} type={nal_type} bytes={prefix:02x?}",
                packet.header.sequence,
            );
        }
        let completed = match nal_type {
            1..=23 => {
                // Single NAL unit.
                if self.fragment.is_some() {
                    self.assembler.reset();
                }
                self.fragment = None;
                // Apple also sends an `avc1` sample entry containing `avcC`
                // as a standalone RTP unit immediately before the IDR. Its
                // first byte is not a usable NAL header, so extract SPS/PPS
                // here just as we do for the first STAP-B aggregate item.
                let units = avcc_parameter_sets(payload).unwrap_or_else(|| vec![payload.to_vec()]);
                self.assembler.push(ts, None, units, marker)?
            }
            28 | 29 => {
                // FU-A / FU-B.
                if payload.len() < 2 {
                    return Err(Error::Invalid("H.264 FU packet too short"));
                }
                let start = payload[1] & 0x80 != 0;
                let end = payload[1] & 0x40 != 0;
                let data_offset = if nal_type == 29 {
                    if payload.len() < 4 {
                        return Err(Error::Invalid("H.264 FU-B packet too short"));
                    }
                    4
                } else {
                    2
                };
                let decode_order_number =
                    (nal_type == 29).then(|| u16::from_be_bytes([payload[2], payload[3]]));
                let nal_header = [payload[0] & 0xe0 | (payload[1] & 0x1f)];
                if start {
                    if self.fragment.is_some() {
                        self.assembler.reset();
                    }
                    self.fragment = Some(FragmentBuffer::new(
                        packet.header.sequence,
                        ts,
                        decode_order_number,
                        nal_header.to_vec(),
                        &payload[data_offset..],
                    ));
                } else if let Some(fragment) = &mut self.fragment
                    && !fragment.append(packet.header.sequence, ts, &payload[data_offset..])
                {
                    self.fragment = None;
                    self.assembler.reset();
                }
                let mut completed = None;
                if end && let Some(fragment) = self.fragment.take() {
                    let decode_order_number = decode_order_number.or(fragment.decode_order_number);
                    completed = self.assembler.push(
                        ts,
                        decode_order_number,
                        vec![fragment.finish()],
                        marker,
                    )?;
                }
                completed
            }
            24 => {
                // STAP-A: multiple NAL units in one packet.
                if self.fragment.is_some() {
                    self.assembler.reset();
                }
                if payload.len() < 3 {
                    return Err(Error::Invalid("H.264 STAP-A packet too short"));
                }
                self.fragment = None;
                let mut units = Vec::new();
                let mut offset = 1;
                while offset < payload.len() {
                    if offset + 2 > payload.len() {
                        return Err(Error::Invalid("H.264 STAP-A truncated"));
                    }
                    let nal_len =
                        usize::from(u16::from_be_bytes([payload[offset], payload[offset + 1]]));
                    offset += 2;
                    if offset + nal_len > payload.len() {
                        return Err(Error::Invalid("H.264 STAP-A NAL length"));
                    }
                    units.push(payload[offset..offset + nal_len].to_vec());
                    offset += nal_len;
                }
                self.assembler.push(ts, None, units, marker)?
            }
            25 => {
                // STAP-B starts with a two-byte decoding-order number. Apple
                // uses its first aggregate item for an `avc1` sample entry;
                // extract the nested avcC SPS/PPS before the following IDR.
                if self.fragment.is_some() {
                    self.assembler.reset();
                }
                if payload.len() < 5 {
                    return Err(Error::Invalid("H.264 STAP-B packet too short"));
                }
                self.fragment = None;
                let mut units = Vec::new();
                let mut offset = 3;
                while offset < payload.len() {
                    if offset + 2 > payload.len() {
                        return Err(Error::Invalid("H.264 STAP-B truncated"));
                    }
                    let nal_len =
                        usize::from(u16::from_be_bytes([payload[offset], payload[offset + 1]]));
                    offset += 2;
                    let end = offset
                        .checked_add(nal_len)
                        .filter(|end| *end <= payload.len())
                        .ok_or(Error::Invalid("H.264 STAP-B NAL length"))?;
                    let item = &payload[offset..end];
                    if item
                        .first()
                        .is_some_and(|byte| byte & 0x80 == 0 && matches!(byte & 0x1f, 1..=23))
                    {
                        units.push(item.to_vec());
                    } else if let Some(parameter_sets) = avcc_parameter_sets(item) {
                        units.extend(parameter_sets);
                    }
                    offset = end;
                }
                if units.is_empty() {
                    return Err(Error::Invalid("H.264 STAP-B has no decodable NAL units"));
                }
                let decode_order_number = Some(u16::from_be_bytes([payload[1], payload[2]]));
                self.assembler
                    .push(ts, decode_order_number, units, marker)?
            }
            _ => {
                // Unknown type: treat as a complete single unit and move on.
                self.fragment = None;
                self.assembler
                    .push(ts, None, vec![payload.to_vec()], marker)?
            }
        };
        Ok(completed)
    }
}

fn avcc_parameter_sets(bytes: &[u8]) -> Option<Vec<Vec<u8>>> {
    for marker in 4..bytes.len().saturating_sub(3) {
        if bytes.get(marker..marker + 4) != Some(b"avcC") {
            continue;
        }
        let box_start = marker - 4;
        let box_size = usize::try_from(u32::from_be_bytes(
            bytes.get(box_start..marker)?.try_into().ok()?,
        ))
        .ok()?;
        let box_end = box_start.checked_add(box_size)?;
        if box_size < 8 || box_end > bytes.len() {
            continue;
        }
        if let Some(parameter_sets) = parse_avcc_record(&bytes[marker + 4..box_end]) {
            return Some(parameter_sets);
        }
    }
    None
}

fn parse_avcc_record(record: &[u8]) -> Option<Vec<Vec<u8>>> {
    if record.len() < 7 || record[0] != 1 {
        return None;
    }
    let mut position = 5;
    let sps_count = usize::from(*record.get(position)? & 0x1f);
    position += 1;
    let mut parameter_sets = Vec::with_capacity(sps_count.saturating_add(1));
    for _ in 0..sps_count {
        let length = usize::from(u16::from_be_bytes(
            record.get(position..position + 2)?.try_into().ok()?,
        ));
        position += 2;
        let end = position.checked_add(length)?;
        let parameter_set = record.get(position..end)?;
        if parameter_set.first().map(|byte| byte & 0x1f) != Some(7) {
            return None;
        }
        parameter_sets.push(parameter_set.to_vec());
        position = end;
    }
    let pps_count = usize::from(*record.get(position)?);
    position += 1;
    for _ in 0..pps_count {
        let length = usize::from(u16::from_be_bytes(
            record.get(position..position + 2)?.try_into().ok()?,
        ));
        position += 2;
        let end = position.checked_add(length)?;
        let parameter_set = record.get(position..end)?;
        if parameter_set.first().map(|byte| byte & 0x1f) != Some(8) {
            return None;
        }
        parameter_sets.push(parameter_set.to_vec());
        position = end;
    }
    (sps_count > 0 && pps_count > 0).then_some(parameter_sets)
}

/// HEVC RTP depacketizer (RFC 7798).
pub struct HevcDepacketizer {
    fragment: Option<FragmentBuffer>,
    assembler: FrameAssembler,
    donl_present: bool,
}

impl Default for HevcDepacketizer {
    fn default() -> Self {
        Self::new()
    }
}

impl HevcDepacketizer {
    pub fn new() -> Self {
        Self {
            fragment: None,
            assembler: FrameAssembler::new(),
            donl_present: false,
        }
    }

    /// Build the Apple AVC variant, which carries a two-byte decoding-order
    /// number after every HEVC payload header.
    pub fn new_with_donl() -> Self {
        Self {
            fragment: None,
            assembler: FrameAssembler::new(),
            donl_present: true,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.fragment = None;
        self.assembler.reset();
    }

    pub fn push(&mut self, packet: &RtpPacket<'_>) -> Result<Option<AccessUnit>> {
        let payload = packet.payload;
        if payload.is_empty() {
            return Ok(None);
        }
        let nal_type = (payload[0] >> 1) & 0x3f;
        let marker = packet.header.marker;
        let ts = packet.header.timestamp;
        if std::env::var_os("ARD_RTP_WIRE_TRACE").is_some() && nal_type == 49 {
            let prefix = &payload[..payload.len().min(9)];
            eprintln!(
                "HEVC FU wire: seq={} ts={ts} marker={marker} start={} end={} bytes={prefix:02x?}",
                packet.header.sequence,
                payload.get(2).is_some_and(|byte| byte & 0x80 != 0),
                payload.get(2).is_some_and(|byte| byte & 0x40 != 0),
            );
        }
        let completed = match nal_type {
            0..=31 => {
                // Single NAL unit (type <= 31 excludes aggregation/fragmentation).
                if self.fragment.is_some() {
                    self.assembler.reset();
                }
                self.fragment = None;
                let decode_order_number = if self.donl_present {
                    if payload.len() < 4 {
                        return Err(Error::Invalid("HEVC single NAL missing DONL"));
                    }
                    Some(u16::from_be_bytes([payload[2], payload[3]]))
                } else {
                    None
                };
                let unit = if self.donl_present {
                    let mut unit = Vec::with_capacity(payload.len() - 2);
                    unit.extend_from_slice(&payload[..2]);
                    unit.extend_from_slice(&payload[4..]);
                    unit
                } else {
                    payload.to_vec()
                };
                self.assembler
                    .push(ts, decode_order_number, vec![unit], marker)?
            }
            48 => {
                // Aggregation packet (AP).
                if self.fragment.is_some() {
                    self.assembler.reset();
                }
                if payload.len() < 2 {
                    return Err(Error::Invalid("HEVC AP packet too short"));
                }
                self.fragment = None;
                let mut units = Vec::new();
                let decode_order_number = if self.donl_present {
                    if payload.len() < 4 {
                        return Err(Error::Invalid("HEVC AP missing DONL"));
                    }
                    Some(u16::from_be_bytes([payload[2], payload[3]]))
                } else {
                    None
                };
                let mut offset = if self.donl_present {
                    if payload.len() < 4 {
                        return Err(Error::Invalid("HEVC AP missing DONL"));
                    }
                    4
                } else {
                    2
                };
                while offset < payload.len() {
                    if offset + 2 > payload.len() {
                        return Err(Error::Invalid("HEVC AP truncated"));
                    }
                    let nal_len =
                        usize::from(u16::from_be_bytes([payload[offset], payload[offset + 1]]));
                    offset += 2;
                    if offset + nal_len > payload.len() {
                        return Err(Error::Invalid("HEVC AP NAL length"));
                    }
                    units.push(payload[offset..offset + nal_len].to_vec());
                    offset += nal_len;
                }
                self.assembler
                    .push(ts, decode_order_number, units, marker)?
            }
            49 => {
                // Fragmentation unit (FU).
                let data_offset = if self.donl_present { 5 } else { 3 };
                if payload.len() < data_offset {
                    return Err(Error::Invalid("HEVC FU packet too short"));
                }
                let start = payload[2] & 0x80 != 0;
                let end = payload[2] & 0x40 != 0;
                let decode_order_number = self
                    .donl_present
                    .then(|| u16::from_be_bytes([payload[3], payload[4]]));
                let mut header = [0u8; 2];
                header[0] = payload[0] & 0x81 | ((payload[2] & 0x3f) << 1);
                header[1] = payload[1];
                if start {
                    if self.fragment.is_some() {
                        self.assembler.reset();
                    }
                    self.fragment = Some(FragmentBuffer::new(
                        packet.header.sequence,
                        ts,
                        decode_order_number,
                        header.to_vec(),
                        &payload[data_offset..],
                    ));
                } else if let Some(fragment) = &mut self.fragment
                    && !fragment.append(packet.header.sequence, ts, &payload[data_offset..])
                {
                    self.fragment = None;
                    self.assembler.reset();
                }
                let mut completed = None;
                if end && let Some(fragment) = self.fragment.take() {
                    let decode_order_number = decode_order_number.or(fragment.decode_order_number);
                    completed = self.assembler.push(
                        ts,
                        decode_order_number,
                        vec![fragment.finish()],
                        marker,
                    )?;
                }
                completed
            }
            50 => return Err(Error::Invalid("unsupported HEVC PACI packet")),
            _ => {
                self.fragment = None;
                self.assembler
                    .push(ts, None, vec![payload.to_vec()], marker)?
            }
        };
        Ok(completed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rtp(seq: u16, ts: u32, marker: bool, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(12 + payload.len());
        out.push(0x80);
        out.push(96 | if marker { 0x80 } else { 0 });
        out.extend_from_slice(&seq.to_be_bytes());
        out.extend_from_slice(&ts.to_be_bytes());
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn parses_rtp_header_and_extension() {
        let packet = rtp(0x1234, 0x0102_0304, true, &[1, 2, 3]);
        let parsed = RtpPacket::parse(&packet).expect("parses");
        assert_eq!(parsed.header.sequence, 0x1234);
        assert_eq!(parsed.header.timestamp, 0x0102_0304);
        assert!(parsed.header.marker);
        assert_eq!(parsed.payload, &[1, 2, 3]);
    }

    #[test]
    fn parses_extension_and_encrypted_padding_boundaries() {
        let mut packet = vec![0xb0, 0xe4, 0x00, 0x09, 0, 0, 0, 2, 0, 0, 0, 3];
        packet.extend_from_slice(&[0xbe, 0xde, 0, 1, 0xaa, 0xbb, 0xcc, 0xdd]);
        packet.extend_from_slice(&[1, 2, 3, 0, 0, 3]);

        let encrypted = RtpPacket::parse_encrypted(&packet).expect("encrypted parse");
        assert_eq!(encrypted.header.payload_type, 100);
        assert_eq!(encrypted.extension_profile, Some(0xbede));
        assert_eq!(encrypted.extension_data, &[0xaa, 0xbb, 0xcc, 0xdd]);
        assert_eq!(encrypted.payload, &[1, 2, 3, 0, 0, 3]);

        let parsed = RtpPacket::parse(&packet).expect("clear parse");
        assert_eq!(parsed.payload, &[1, 2, 3]);
        assert_eq!(parsed.padding_len, 3);
    }

    #[test]
    fn reorders_authenticated_packets_before_depacketization() {
        let first = rtp(10, 4000, false, &[0x41, 1]);
        let marker = rtp(12, 4000, true, &[0x41, 3]);
        let middle = rtp(11, 4000, false, &[0x41, 2]);
        let mut reorder = RtpReorderBuffer::with_codec(MediaStreamCodec::H264);
        assert!(reorder.push(&first).expect("first packet").is_empty());
        assert!(reorder.push(&middle).expect("middle packet").is_empty());
        let ready = reorder.push(&marker).expect("marker packet");
        let sequences: Vec<_> = ready
            .iter()
            .map(|packet| {
                RtpPacket::parse(packet)
                    .expect("parsed packet")
                    .header
                    .sequence
            })
            .collect();
        assert_eq!(sequences, vec![10, 11, 12]);
    }

    #[test]
    fn reorders_sequence_wrap_within_one_burst() {
        let before_wrap = rtp(u16::MAX, 5000, false, &[0x41, 1]);
        let after_wrap = rtp(0, 5000, false, &[0x41, 3]);
        let between = rtp(1, 5000, true, &[0x41, 2]);
        let mut reorder = RtpReorderBuffer::with_codec(MediaStreamCodec::H264);
        reorder.push(&after_wrap).expect("after-wrap packet");
        reorder.push(&before_wrap).expect("before-wrap packet");
        let ready = reorder.push(&between).expect("marker packet");
        let sequences: Vec<_> = ready
            .iter()
            .map(|packet| {
                RtpPacket::parse(packet)
                    .expect("parsed packet")
                    .header
                    .sequence
            })
            .collect();
        assert_eq!(sequences, vec![u16::MAX, 0, 1]);
    }

    #[test]
    fn waits_for_fu_start_when_marker_arrives_first() {
        let start = rtp(10, 6000, false, &[0x7c, 0x85, 1]);
        let middle = rtp(11, 6000, false, &[0x7c, 0x05, 2]);
        let end = rtp(12, 6000, true, &[0x7c, 0x45, 3]);
        let mut reorder = RtpReorderBuffer::with_codec(MediaStreamCodec::H264);
        assert!(reorder.push(&end).expect("end packet").is_empty());
        assert!(reorder.push(&start).expect("start packet").is_empty());
        let ready = reorder.push(&middle).expect("middle packet");
        let sequences: Vec<_> = ready
            .iter()
            .map(|packet| {
                RtpPacket::parse(packet)
                    .expect("parsed packet")
                    .header
                    .sequence
            })
            .collect();
        assert_eq!(sequences, vec![10, 11, 12]);
    }

    #[test]
    fn drops_late_packet_after_a_burst_was_released() {
        let first = rtp(10, 5000, true, &[0x41, 1]);
        let late = rtp(9, 5000, false, &[0x41, 0]);
        let next = rtp(11, 5001, true, &[0x41, 2]);
        let mut reorder = RtpReorderBuffer::with_codec(MediaStreamCodec::H264);
        assert_eq!(reorder.push(&first).expect("first").len(), 1);
        assert!(reorder.push(&late).expect("late").is_empty());
        let ready = reorder.push(&next).expect("next");
        assert_eq!(ready.len(), 1);
        assert_eq!(
            RtpPacket::parse(&ready[0]).expect("packet").header.sequence,
            11
        );
    }

    #[test]
    fn discards_a_marker_burst_with_a_missing_packet_when_next_frame_arrives() {
        let first = rtp(10, 5000, false, &[0x41, 1]);
        let marker = rtp(12, 5000, true, &[0x41, 3]);
        let next = rtp(13, 5001, true, &[0x65, 4]);
        let mut reorder = RtpReorderBuffer::with_codec(MediaStreamCodec::H264);

        assert!(reorder.push(&first).expect("first").is_empty());
        assert!(reorder.push(&marker).expect("gapped marker").is_empty());
        let ready = reorder.push(&next).expect("next frame");

        assert_eq!(reorder.take_dropped_access_units(), 1);
        assert_eq!(ready.len(), 1);
        let packet = RtpPacket::parse(&ready[0]).expect("next packet");
        assert_eq!(packet.header.timestamp, 5001);
        assert_eq!(packet.header.sequence, 13);
    }

    #[test]
    fn missing_marker_packet_cannot_hold_the_reorder_queue_until_capacity() {
        let old = rtp(20, 6000, false, &[0x41, 1]);
        let next_start = rtp(22, 6001, false, &[0x41, 2]);
        let next_marker = rtp(23, 6001, true, &[0x41, 3]);
        let mut reorder = RtpReorderBuffer::with_codec(MediaStreamCodec::H264);

        assert!(reorder.push(&old).expect("old frame").is_empty());
        assert!(
            reorder
                .push(&next_start)
                .expect("next frame start")
                .is_empty()
        );
        let ready = reorder.push(&next_marker).expect("next marker");

        assert_eq!(reorder.take_dropped_access_units(), 1);
        let sequences = ready
            .iter()
            .map(|packet| RtpPacket::parse(packet).expect("packet").header.sequence)
            .collect::<Vec<_>>();
        assert_eq!(sequences, vec![22, 23]);
    }

    #[test]
    fn releases_hevc_fu_larger_than_reorder_window() {
        let mut reorder = RtpReorderBuffer::with_codec(MediaStreamCodec::Hevc);
        let mut depacketizer = HevcDepacketizer::new();
        let mut completed = None;

        for index in 0..70u16 {
            let start = index == 0;
            let end = index == 69;
            let fu_header = 1 | if start { 0x80 } else { 0 } | if end { 0x40 } else { 0 };
            let datagram = rtp(
                1000 + index,
                9000,
                end,
                &[0x62, 0x01, fu_header, index as u8],
            );
            for ready in reorder.push(&datagram).expect("reorder") {
                completed = depacketizer
                    .push(&RtpPacket::parse(&ready).expect("packet"))
                    .expect("depacketize")
                    .or(completed);
            }
        }

        let frame = completed.expect("large fragmented frame completes");
        assert_eq!(frame.nal_units.len(), 1);
        assert_eq!(frame.nal_units[0].len(), 72);
        assert_eq!(&frame.nal_units[0][2..], &(0..70u8).collect::<Vec<_>>());
    }

    #[test]
    fn reassembles_h264_fu_a() {
        let nal = [0x65, 0x88, 0x84, 0x21, 0x22, 0x33, 0x44];
        // FU-A: type 28, start of first part.
        let part1 = [0x7c, 0x85, 0x88, 0x84];
        let part2 = [0x7c, 0x45, 0x21, 0x22, 0x33, 0x44];
        let mut depacketizer = H264Depacketizer::new();
        let datagram1 = rtp(1, 1000, false, &part1);
        let datagram2 = rtp(2, 1000, true, &part2);
        let p1 = RtpPacket::parse(&datagram1).expect("parses");
        assert!(depacketizer.push(&p1).expect("push").is_none());
        let p2 = RtpPacket::parse(&datagram2).expect("parses");
        let frame = depacketizer
            .push(&p2)
            .expect("push")
            .expect("frame completes");
        assert_eq!(frame.nal_units.len(), 1);
        assert_eq!(frame.nal_units[0], nal);
        assert!(frame.is_idr());
    }

    #[test]
    fn reassembles_h264_fu_b_followed_by_fu_a() {
        let nal = [0x65, 0x88, 0x84, 0x21, 0x22, 0x33, 0x44];
        let part1 = [0x7d, 0x85, 0x12, 0x34, 0x88, 0x84];
        let part2 = [0x7c, 0x45, 0x21, 0x22, 0x33, 0x44];
        let mut depacketizer = H264Depacketizer::new();
        let first = rtp(10, 1100, false, &part1);
        let second = rtp(11, 1100, true, &part2);
        let first = RtpPacket::parse(&first).expect("parses");
        assert!(depacketizer.push(&first).expect("push").is_none());
        let second = RtpPacket::parse(&second).expect("parses");
        let frame = depacketizer
            .push(&second)
            .expect("push")
            .expect("frame completes");
        assert_eq!(frame.decode_order_number, Some(0x1234));
        assert_eq!(frame.nal_units, vec![nal.to_vec()]);
    }

    #[test]
    fn reassembles_hevc_fu() {
        let nal = [
            0x42, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03,
        ];
        // FU type 49: header = [type=49<<1|0, layer], FU header start/end.
        let part1 = [0x62, 0x01, 0x80 | 0x21, 0x01, 0x60, 0x00, 0x00];
        let part2 = [0x62, 0x01, 0x40 | 0x21, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03];
        let mut depacketizer = HevcDepacketizer::new();
        let datagram1 = rtp(1, 2000, false, &part1);
        let datagram2 = rtp(2, 2000, true, &part2);
        let p1 = RtpPacket::parse(&datagram1).expect("parses");
        assert!(depacketizer.push(&p1).expect("push").is_none());
        let p2 = RtpPacket::parse(&datagram2).expect("parses");
        let frame = depacketizer.push(&p2).expect("push").expect("frame");
        assert_eq!(frame.nal_units.len(), 1);
        assert_eq!(frame.nal_units[0], nal);
    }

    #[test]
    fn reassembles_apple_hevc_donl_access_unit() {
        let aggregation = [
            0x60, 0x01, 0x12, 0x34, // AP header + DONL
            0x00, 0x03, 0x40, 0x01, 0xaa, // VPS
            0x00, 0x03, 0x42, 0x01, 0xbb, // SPS
            0x00, 0x03, 0x44, 0x01, 0xcc, // PPS
        ];
        let fragment_start = [0x62, 0x01, 0x80 | 20, 0x12, 0x34, 0xde, 0xad];
        let fragment_end = [0x62, 0x01, 0x40 | 20, 0x12, 0x34, 0xbe, 0xef];
        let mut depacketizer = HevcDepacketizer::new_with_donl();
        let aggregation = rtp(20, 7000, false, &aggregation);
        let fragment_start = rtp(21, 7000, false, &fragment_start);
        let fragment_end = rtp(22, 7000, true, &fragment_end);
        assert!(
            depacketizer
                .push(&RtpPacket::parse(&aggregation).expect("AP"))
                .expect("push AP")
                .is_none()
        );
        assert!(
            depacketizer
                .push(&RtpPacket::parse(&fragment_start).expect("FU start"))
                .expect("push FU start")
                .is_none()
        );
        let frame = depacketizer
            .push(&RtpPacket::parse(&fragment_end).expect("FU end"))
            .expect("push FU end")
            .expect("frame");
        assert_eq!(frame.decode_order_number, Some(0x1234));
        assert_eq!(
            frame.nal_units,
            vec![
                vec![0x40, 0x01, 0xaa],
                vec![0x42, 0x01, 0xbb],
                vec![0x44, 0x01, 0xcc],
                vec![0x28, 0x01, 0xde, 0xad, 0xbe, 0xef],
            ]
        );
    }

    #[test]
    fn stapa_combines_units() {
        let payload = [
            0x78, 0x00, 0x03, 0x67, 0x42, 0x00, 0x00, 0x04, 0x68, 0xce, 0x3c, 0x80,
        ];
        let mut depacketizer = H264Depacketizer::new();
        let datagram = rtp(9, 3000, true, &payload);
        let packet = RtpPacket::parse(&datagram).expect("parses");
        let frame = depacketizer.push(&packet).expect("push").expect("frame");
        assert_eq!(frame.nal_units.len(), 2);
        assert_eq!(frame.nal_units[0], &[0x67, 0x42, 0x00]);
        assert_eq!(frame.nal_units[1], &[0x68, 0xce, 0x3c, 0x80]);
    }

    #[test]
    fn stapb_extracts_apple_avcc_parameter_sets_and_idr() {
        let sps = [0x67, 0x64, 0x00];
        let pps = [0x68, 0xce, 0x3c, 0x80];
        let idr = [0x65, 0x88, 0x84];
        let avcc_record = [
            1, 100, 0, 52, 0xff, 0xe1, 0, 3, 0x67, 0x64, 0, 1, 0, 4, 0x68, 0xce, 0x3c, 0x80,
        ];
        let mut sample_entry = vec![0x92, 0xe6, 0xc0, 0xa3];
        sample_entry.extend_from_slice(&(8_u32 + avcc_record.len() as u32).to_be_bytes());
        sample_entry.extend_from_slice(b"avcC");
        sample_entry.extend_from_slice(&avcc_record);

        let mut payload = vec![0x39, 0x12, 0x34];
        payload.extend_from_slice(&(sample_entry.len() as u16).to_be_bytes());
        payload.extend_from_slice(&sample_entry);
        payload.extend_from_slice(&(idr.len() as u16).to_be_bytes());
        payload.extend_from_slice(&idr);

        let mut depacketizer = H264Depacketizer::new();
        let datagram = rtp(10, 4000, true, &payload);
        let packet = RtpPacket::parse(&datagram).expect("parses");
        let frame = depacketizer.push(&packet).expect("push").expect("frame");
        assert_eq!(frame.decode_order_number, Some(0x1234));
        assert_eq!(
            frame.nal_units,
            vec![sps.to_vec(), pps.to_vec(), idr.to_vec()]
        );
        assert!(frame.is_idr());
    }

    #[test]
    fn standalone_apple_avc1_entry_extracts_parameter_sets_before_idr() {
        let sps = [0x67, 0x64, 0x00, 0x34];
        let pps = [0x68, 0xee, 0x3c, 0xb0];
        let mut sample_entry = vec![0x92, 0xe6, 0xc0, 0xa3];
        let avcc_size = 8 + 6 + 2 + sps.len() + 1 + 2 + pps.len();
        sample_entry.extend_from_slice(&(avcc_size as u32).to_be_bytes());
        sample_entry.extend_from_slice(b"avcC");
        sample_entry.extend_from_slice(&[1, 100, 0, 52, 0xff, 0xe1]);
        sample_entry.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        sample_entry.extend_from_slice(&sps);
        sample_entry.push(1);
        sample_entry.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        sample_entry.extend_from_slice(&pps);

        let first = rtp(10, 700, false, &sample_entry);
        let idr = rtp(11, 700, true, &[0x65, 0xaa, 0xbb]);
        let mut depacketizer = H264Depacketizer::new();
        assert!(
            depacketizer
                .push(&RtpPacket::parse(&first).expect("sample entry"))
                .unwrap()
                .is_none()
        );
        let unit = depacketizer
            .push(&RtpPacket::parse(&idr).expect("IDR"))
            .unwrap()
            .unwrap();
        assert_eq!(
            unit.nal_units,
            vec![sps.to_vec(), pps.to_vec(), vec![0x65, 0xaa, 0xbb]]
        );
    }
}
