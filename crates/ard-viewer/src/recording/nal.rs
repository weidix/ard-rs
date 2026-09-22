//! H.264 bitstream helpers shared by the platform encoders.
//!
//! VideoToolbox hands out length-prefixed (AVCC) access units that go straight
//! into an `avcC`-described MP4 track, while Media Foundation hands out the
//! Annex-B byte stream. Both encoders also publish their SPS/PPS differently, so
//! the conversions live here and are unit-tested without touching a codec.

/// One H.264 NAL unit carved out of a byte stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NalUnit<'a> {
    /// NAL header byte, whose low five bits are `nal_unit_type`.
    pub header: u8,
    /// Whole NAL unit including its header byte.
    pub bytes: &'a [u8],
}

impl NalUnit<'_> {
    pub fn unit_type(&self) -> u8 {
        self.header & 0x1f
    }

    /// An instantaneous decoder refresh picture. Its slice header starts a new
    /// prediction chain, so it is the only sample type an MP4 may mark as sync.
    pub fn is_idr(&self) -> bool {
        self.unit_type() == 5
    }
}

/// Iterate the Annex-B NAL units of `data`.
///
/// A leading start code is optional. Trailing bytes that never reach a start
/// code are returned as the final unit, which is what an encoder's last sample
/// looks like when the stream is trimmed at a frame boundary.
pub(crate) fn annex_b_units(data: &[u8]) -> Vec<NalUnit<'_>> {
    let mut units = Vec::new();
    let mut cursor = 0;
    let mut start: Option<usize> = None;
    while cursor < data.len() {
        if let Some(prefix) = start_code_len(&data[cursor..]) {
            if let Some(begin) = start
                && begin < cursor
            {
                push_unit(&mut units, &data[begin..cursor]);
            }
            cursor += prefix;
            start = Some(cursor);
        } else {
            cursor += 1;
        }
    }
    if let Some(begin) = start
        && begin < data.len()
    {
        push_unit(&mut units, &data[begin..]);
    }
    units
}

fn push_unit<'a>(units: &mut Vec<NalUnit<'a>>, bytes: &'a [u8]) {
    let Some((&header, _)) = bytes.split_first() else {
        return;
    };
    units.push(NalUnit { header, bytes });
}

/// Length of the Annex-B start code at the beginning of `data`, if any.
fn start_code_len(data: &[u8]) -> Option<usize> {
    match data {
        [0, 0, 1, ..] => Some(3),
        [0, 0, 0, 1, ..] => Some(4),
        _ => None,
    }
}

/// Rewrite an Annex-B access unit as a 4-byte-length-prefixed AVCC sample.
///
/// Returns `None` when `data` holds no complete NAL unit, so a malformed
/// encoder output can never be written into the container as an empty sample.
pub(crate) fn to_avcc(data: &[u8]) -> Option<Vec<u8>> {
    to_avcc_skipping(data, |_| false)
}

/// Rewrite an access unit, dropping the NAL types `skip` selects.
///
/// Media Foundation prepends parameter sets to its byte stream and publishes
/// them separately as well; an MP4 track carries them in its configuration box,
/// so in-band copies are removed rather than written twice.
pub(crate) fn to_avcc_skipping(data: &[u8], skip: impl Fn(u8) -> bool) -> Option<Vec<u8>> {
    let units = annex_b_units(data);
    let mut output = Vec::with_capacity(data.len() + units.len() * 4);
    let mut written = 0_usize;
    for unit in units {
        if skip(unit.unit_type()) {
            continue;
        }
        let length = u32::try_from(unit.bytes.len()).ok()?;
        output.extend_from_slice(&length.to_be_bytes());
        output.extend_from_slice(unit.bytes);
        written += 1;
    }
    (written > 0).then_some(output)
}

/// Whether an Annex-B access unit contains an IDR picture.
pub(crate) fn annex_b_is_sync(data: &[u8]) -> bool {
    annex_b_units(data).iter().any(NalUnit::is_idr)
}

/// Split a codec's parameter sets out of an access unit or sequence header.
///
/// `is_hevc` selects the HEVC NAL type mapping; the recording pipeline is
/// H.264 only, but keeping the branch explicit avoids a silent mis-parse if a
/// HEVC encoder backend is added next to it.
pub(crate) fn parameter_sets(data: &[u8], is_hevc: bool) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let mut sps = None;
    let mut pps = None;
    for unit in annex_b_units(data) {
        let unit_type = if is_hevc {
            (unit.header >> 1) & 0x3f
        } else {
            unit.unit_type()
        };
        match unit_type {
            33 if is_hevc => sps = Some(unit.bytes.to_vec()),
            34 if is_hevc => pps = Some(unit.bytes.to_vec()),
            7 if !is_hevc => sps = Some(unit.bytes.to_vec()),
            8 if !is_hevc => pps = Some(unit.bytes.to_vec()),
            _ => {}
        }
    }
    (sps, pps)
}

#[cfg(test)]
mod tests {
    use super::{annex_b_is_sync, annex_b_units, parameter_sets, to_avcc, to_avcc_skipping};

    /// SPS, PPS, and an IDR slice, as an encoder emits them for a keyframe.
    fn annex_b_keyframe() -> Vec<u8> {
        let mut data = Vec::new();
        for (unit_type, payload) in [
            (7_u8, &[0xaa_u8, 0xbb][..]),
            (8, &[0xcc]),
            (5, &[0xdd, 0xee]),
        ] {
            data.extend_from_slice(&[0, 0, 0, 1, 0x60 | unit_type]);
            data.extend_from_slice(payload);
        }
        data
    }

    #[test]
    fn annex_b_units_accept_three_and_four_byte_start_codes() {
        let data = [0, 0, 1, 0x65, 0x11, 0, 0, 0, 1, 0x41, 0x22];
        let units = annex_b_units(&data);
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].bytes, &[0x65, 0x11]);
        assert_eq!(units[1].bytes, &[0x41, 0x22]);
    }

    #[test]
    fn avcc_conversion_length_prefixes_every_unit() {
        let data = annex_b_keyframe();
        let avcc = to_avcc(&data).expect("keyframe converts");
        assert_eq!(
            avcc,
            vec![
                0, 0, 0, 3, 0x67, 0xaa, 0xbb, //
                0, 0, 0, 2, 0x68, 0xcc, //
                0, 0, 0, 3, 0x65, 0xdd, 0xee,
            ]
        );
    }

    #[test]
    fn avcc_conversion_rejects_input_without_nals() {
        assert!(to_avcc(&[]).is_none());
        assert!(to_avcc(&[0, 0, 0, 0]).is_none());
    }

    #[test]
    fn filtered_conversion_drops_parameter_sets_from_the_sample() {
        let data = annex_b_keyframe();
        let avcc = to_avcc_skipping(&data, |unit_type| unit_type == 7 || unit_type == 8)
            .expect("the IDR survives");
        assert_eq!(avcc, vec![0, 0, 0, 3, 0x65, 0xdd, 0xee]);
        // A unit that held nothing but parameter sets is not a sample at all.
        let (sps, _) = parameter_sets(&data, false);
        let sets = sps.expect("sps");
        let mut only_sets = vec![0, 0, 0, 1];
        only_sets.extend_from_slice(&sets);
        assert!(to_avcc_skipping(&only_sets, |unit_type| unit_type == 7).is_none());
    }

    #[test]
    fn sync_detection_requires_an_idr_slice() {
        assert!(annex_b_is_sync(&annex_b_keyframe()));
        let non_idr = [0, 0, 1, 0x67, 0xaa, 0, 0, 1, 0x41, 0xbb];
        assert!(!annex_b_is_sync(&non_idr));
    }

    #[test]
    fn parameter_sets_are_split_without_start_codes() {
        let (sps, pps) = parameter_sets(&annex_b_keyframe(), false);
        assert_eq!(sps, Some(vec![0x67, 0xaa, 0xbb]));
        assert_eq!(pps, Some(vec![0x68, 0xcc]));
    }

    #[test]
    fn hevc_parameter_sets_use_the_hevc_type_mapping() {
        // HEVC VPS/SPS/PPS are types 32/33/34 behind a two-byte header.
        let data = [
            0, 0, 0, 1, 0x42, 0x01, 0x07, 0, 0, 0, 1, 0x44, 0x01, 0x08, 0, 0, 0, 1, 0x26, 0x01,
            0x09,
        ];
        let (sps, pps) = parameter_sets(&data, true);
        assert_eq!(sps, Some(vec![0x42, 0x01, 0x07]));
        assert_eq!(pps, Some(vec![0x44, 0x01, 0x08]));
    }
}
