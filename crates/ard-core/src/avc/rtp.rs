//! RTP receive path for the AVC media stream.
//!
//! The server pushes H.264 (payload types 123/126) or HEVC (payload type
//! 100) over UDP as RFC 3550 RTP packets. This module parses the RTP header,
//! strips SRTP (see [`crate::avc::srtp`]), reassembles fragmented NAL units
//! (RFC 6184 FU-A/STAP-A for H.264, RFC 7798 FU/AP for HEVC) and emits whole
//! access units for the platform decoder.

use crate::{Error, Result};

use super::{MAX_NAL_UNITS_PER_ACCESS_UNIT, MAX_RTP_PACKET};

const MAX_ASSEMBLED_NAL_BYTES: usize = 16 * 1024 * 1024;

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
    /// Total bytes consumed from the datagram (header + extensions + payload).
    pub wire_len: usize,
}

impl<'a> RtpPacket<'a> {
    /// Parse an RTP datagram. CSRC and the RFC 3550 extension block are
    /// skipped; the payload is returned as-is (SRTP decryption, when enabled,
    /// must run before parsing).
    pub fn parse(datagram: &'a [u8]) -> Result<Self> {
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
        if extension {
            // RFC 3550 header extension: u16 profile, u16 length (in 32-bit words).
            if offset + 4 > datagram.len() {
                return Err(Error::NeedMore {
                    needed: offset + 4,
                    available: datagram.len(),
                });
            }
            let ext_len = usize::from(u16::from_be_bytes([
                datagram[offset + 2],
                datagram[offset + 3],
            ])) * 4;
            offset = offset
                .checked_add(4)
                .and_then(|v| v.checked_add(ext_len))
                .ok_or(Error::LimitExceeded("RTP extension"))?;
        }
        let payload_end = if padding {
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
            datagram
                .len()
                .checked_sub(pad_len)
                .ok_or(Error::Invalid("RTP padding length"))?
        } else {
            datagram.len()
        };
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
            wire_len: payload_end,
        })
    }
}

/// One decoded NAL unit (without start code or length prefix).
pub type NalUnit = Vec<u8>;

/// A complete access unit ready for the decoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessUnit {
    pub timestamp: u32,
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
        self.nal_units
            .iter()
            .any(|nal| nal.first().is_some_and(|&b| (b & 0x1f) == 5))
    }
}

/// Shared de-fragmentation state.
struct FragmentBuffer {
    bytes: Vec<u8>,
    nal_header: Vec<u8>,
    timestamp: u32,
    expected_next: u16,
}

impl FragmentBuffer {
    fn new(sequence: u16, timestamp: u32, nal_header: Vec<u8>, first_fragment: &[u8]) -> Self {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(first_fragment);
        Self {
            bytes,
            nal_header,
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
            nal_units: Vec::new(),
            bytes: 0,
        }
    }

    fn push(
        &mut self,
        timestamp: u32,
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
                self.bytes = incoming_bytes;
                self.current_timestamp = Some(timestamp);
                self.nal_units = units;
                return Ok(Some(AccessUnit {
                    timestamp: previous_ts,
                    nal_units: previous,
                }));
            }
        } else {
            self.current_timestamp = Some(timestamp);
        }
        self.bytes = self
            .bytes
            .checked_add(incoming_bytes)
            .filter(|size| *size <= MAX_ASSEMBLED_NAL_BYTES)
            .ok_or(Error::LimitExceeded("RTP access-unit size"))?;
        self.nal_units.extend(units);
        if marker {
            let nal_units = std::mem::take(&mut self.nal_units);
            self.current_timestamp = None;
            self.bytes = 0;
            Ok(Some(AccessUnit {
                timestamp,
                nal_units,
            }))
        } else {
            Ok(None)
        }
    }

    /// Flush any buffered NAL units (used when a fragment is lost).
    fn reset(&mut self) {
        self.current_timestamp = None;
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
        let completed = match nal_type {
            1..=23 => {
                // Single NAL unit.
                if self.fragment.is_some() {
                    self.assembler.reset();
                }
                self.fragment = None;
                self.assembler.push(ts, vec![payload.to_vec()], marker)?
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
                let nal_header = [payload[0] & 0xe0 | (payload[1] & 0x1f)];
                if start {
                    if self.fragment.is_some() {
                        self.assembler.reset();
                    }
                    self.fragment = Some(FragmentBuffer::new(
                        packet.header.sequence,
                        ts,
                        nal_header.to_vec(),
                        &payload[data_offset..],
                    ));
                } else if let Some(fragment) = &mut self.fragment
                    && !fragment.append(packet.header.sequence, ts, &payload[2..])
                {
                    self.fragment = None;
                    self.assembler.reset();
                }
                let mut completed = None;
                if end && let Some(fragment) = self.fragment.take() {
                    completed = self.assembler.push(ts, vec![fragment.finish()], marker)?;
                }
                completed
            }
            24 => {
                // STAP-A: multiple NAL units in one packet.
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
                self.assembler.push(ts, units, marker)?
            }
            _ => {
                // Unknown type: treat as a complete single unit and move on.
                self.fragment = None;
                self.assembler.push(ts, vec![payload.to_vec()], marker)?
            }
        };
        Ok(completed)
    }
}

/// HEVC RTP depacketizer (RFC 7798).
pub struct HevcDepacketizer {
    fragment: Option<FragmentBuffer>,
    assembler: FrameAssembler,
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
        }
    }

    pub fn push(&mut self, packet: &RtpPacket<'_>) -> Result<Option<AccessUnit>> {
        let payload = packet.payload;
        if payload.is_empty() {
            return Ok(None);
        }
        let nal_type = (payload[0] >> 1) & 0x3f;
        let marker = packet.header.marker;
        let ts = packet.header.timestamp;
        let completed = match nal_type {
            0..=31 => {
                // Single NAL unit (type <= 31 excludes aggregation/fragmentation).
                if self.fragment.is_some() {
                    self.assembler.reset();
                }
                self.fragment = None;
                self.assembler.push(ts, vec![payload.to_vec()], marker)?
            }
            48 => {
                // Aggregation packet (AP).
                if payload.len() < 2 {
                    return Err(Error::Invalid("HEVC AP packet too short"));
                }
                self.fragment = None;
                let mut units = Vec::new();
                let mut offset = 2;
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
                self.assembler.push(ts, units, marker)?
            }
            49 => {
                // Fragmentation unit (FU).
                if payload.len() < 3 {
                    return Err(Error::Invalid("HEVC FU packet too short"));
                }
                let start = payload[2] & 0x80 != 0;
                let end = payload[2] & 0x40 != 0;
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
                        header.to_vec(),
                        &payload[3..],
                    ));
                } else if let Some(fragment) = &mut self.fragment
                    && !fragment.append(packet.header.sequence, ts, &payload[3..])
                {
                    self.fragment = None;
                    self.assembler.reset();
                }
                let mut completed = None;
                if end && let Some(fragment) = self.fragment.take() {
                    completed = self.assembler.push(ts, vec![fragment.finish()], marker)?;
                }
                completed
            }
            50 => return Err(Error::Invalid("unsupported HEVC PACI packet")),
            _ => {
                self.fragment = None;
                self.assembler.push(ts, vec![payload.to_vec()], marker)?
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
}
