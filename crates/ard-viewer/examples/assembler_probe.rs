#![cfg(target_os = "macos")]
//! Report what one recorded media-stream dump actually carries, then replay it.
//!
//! A dump is only decodable from a keyframe onwards, and a window recorded in
//! the middle of a session can contain none at all: the assembler then reports
//! zero frames, which looks like a broken tool but is the stream's own shape.
//! This walks the packets through the production depacketizer to print the
//! keyframe and parameter-set inventory, then replays them through the
//! production assembler to print how many frames came back.
//!
//! Usage:
//!
//! ```sh
//! cargo run --release -p ard-viewer --example assembler_probe -- DUMP.jsonl
//! ```

use ard_rs::RawStreamIndex;
use ard_rs::media_stream::{
    AVC_VIDEO_SLICE_COUNT, AccessUnit, H264Depacketizer, HevcDepacketizer, MediaStreamCodec,
    RtpPacket, VideoStreamAssembler,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// The two depacketizers share an interface but not a type.
enum Depacketizer {
    Hevc(HevcDepacketizer),
    H264(H264Depacketizer),
}

impl Depacketizer {
    fn new(codec: MediaStreamCodec) -> Self {
        match codec {
            MediaStreamCodec::Hevc => Self::Hevc(HevcDepacketizer::new_with_donl()),
            MediaStreamCodec::H264 => Self::H264(H264Depacketizer::new()),
        }
    }

    fn push(&mut self, packet: &RtpPacket<'_>) -> Result<Option<AccessUnit>, ard_rs::Error> {
        match self {
            Self::Hevc(depacketizer) => depacketizer.push(packet),
            Self::H264(depacketizer) => depacketizer.push(packet),
        }
    }
}

/// NAL unit type of an assembled unit, in each codec's own numbering.
fn nal_type(codec: MediaStreamCodec, unit: &[u8]) -> Option<u8> {
    let first = *unit.first()?;
    Some(match codec {
        MediaStreamCodec::Hevc => (first >> 1) & 0x3f,
        MediaStreamCodec::H264 => first & 0x1f,
    })
}

/// A picture a decoder can start from.
fn is_keyframe(codec: MediaStreamCodec, nal_type: u8) -> bool {
    match codec {
        MediaStreamCodec::Hevc => (16..=21).contains(&nal_type),
        MediaStreamCodec::H264 => nal_type == 5,
    }
}

fn is_parameter_set(codec: MediaStreamCodec, nal_type: u8) -> bool {
    match codec {
        MediaStreamCodec::Hevc => (32..=34).contains(&nal_type),
        MediaStreamCodec::H264 => nal_type == 7 || nal_type == 8,
    }
}

fn nal_name(codec: MediaStreamCodec, nal_type: u8) -> &'static str {
    match (codec, nal_type) {
        (MediaStreamCodec::Hevc, 0) => "TRAIL_N",
        (MediaStreamCodec::Hevc, 1) => "TRAIL_R",
        (MediaStreamCodec::Hevc, 19) => "IDR_W_RADL",
        (MediaStreamCodec::Hevc, 20) => "IDR_N_LP",
        (MediaStreamCodec::Hevc, 21) => "CRA_NUT",
        (MediaStreamCodec::Hevc, 32) => "VPS",
        (MediaStreamCodec::Hevc, 33) => "SPS",
        (MediaStreamCodec::Hevc, 34) => "PPS",
        (MediaStreamCodec::H264, 1) => "slice",
        (MediaStreamCodec::H264, 5) => "IDR",
        (MediaStreamCodec::H264, 7) => "SPS",
        (MediaStreamCodec::H264, 8) => "PPS",
        _ => "",
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let index_path = PathBuf::from(std::env::args().nth(1).ok_or("usage: DUMP.jsonl")?);
    let index = RawStreamIndex::read(&index_path)?;
    let raw = fs::read(RawStreamIndex::raw_path(&index_path))?;
    let codec = if index.header.contains("HEVC") {
        MediaStreamCodec::Hevc
    } else {
        MediaStreamCodec::H264
    };
    let ssrcos: Vec<u32> = index
        .records
        .iter()
        .filter_map(|r| r.ssrc)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    println!(
        "dump: {} records, {} band(s), take {:?} ms, codec {:?}",
        index.records.len(),
        ssrcos.len(),
        index.take_ms,
        codec
    );

    // One pass through the production depacketizer: what does this window hold?
    let mut depacketizers: BTreeMap<u32, Depacketizer> = ssrcos
        .iter()
        .map(|ssrc| (*ssrc, Depacketizer::new(codec)))
        .collect();
    let mut types: BTreeMap<(u32, u8), u64> = BTreeMap::new();
    let mut access_units = 0_u64;
    let mut keyframes: BTreeMap<u32, u64> = BTreeMap::new();
    let mut parameter_sets = 0_u64;
    let mut first_keyframe_ms: Option<u64> = None;
    let mut last_keyframe_ms: Option<u64> = None;
    for record in index.records.iter() {
        let Some(packet) =
            raw.get(record.offset as usize..record.offset as usize + record.length as usize)
        else {
            continue;
        };
        let parsed = RtpPacket::parse(packet)?;
        let Some(depacketizer) = depacketizers.get_mut(&parsed.header.ssrc) else {
            continue;
        };
        let Some(unit) = depacketizer.push(&parsed)? else {
            continue;
        };
        access_units += 1;
        for nal in &unit.nal_units {
            let Some(kind) = nal_type(codec, nal) else {
                continue;
            };
            *types.entry((parsed.header.ssrc, kind)).or_default() += 1;
            if is_parameter_set(codec, kind) {
                parameter_sets += 1;
            }
            if is_keyframe(codec, kind) {
                *keyframes.entry(parsed.header.ssrc).or_default() += 1;
                first_keyframe_ms.get_or_insert(record.t);
                last_keyframe_ms = Some(record.t);
            }
        }
    }
    println!(
        "access units: {access_units}, keyframes: {}, parameter sets: {parameter_sets}",
        keyframes.values().sum::<u64>()
    );
    if let (Some(first), Some(last)) = (first_keyframe_ms, last_keyframe_ms) {
        println!("keyframes arrive at t={first} ms .. {last} ms");
    }
    for ssrc in &ssrcos {
        let mut line = format!("  band ssrc={ssrc}:");
        for ((_, kind), count) in types.range((*ssrc, 0)..=(*ssrc, u8::MAX)) {
            let name = nal_name(codec, *kind);
            if name.is_empty() {
                line.push_str(&format!(" type{kind}={count}"));
            } else {
                line.push_str(&format!(" {name}={count}"));
            }
        }
        println!("{line}");
    }
    if keyframes.values().sum::<u64>() == 0 {
        println!(
            "verdict: this window carries no keyframe, so nothing in it can be decoded on its \
             own. A dump has to include the instant that carries a keyframe; a recording started \
             mid-session may not."
        );
    }

    // Second pass: replay it through the production assembler.
    let mut assembler = VideoStreamAssembler::new(codec);
    for ssrc in &ssrcos {
        assembler.expect_stream(*ssrc);
    }
    let clock = Instant::now();
    let base_ms = index.records.first().map(|r| r.t).unwrap_or(0);
    let mut frames = 0_u64;
    let mut total_losses = 0_u64;
    let mut resets = 0_u64;
    let mut dropped_ts = 0_u64;
    let mut ignored = 0_u64;
    let mut incomplete = 0_u64;
    for record in index.records.iter() {
        let Some(packet) =
            raw.get(record.offset as usize..record.offset as usize + record.length as usize)
        else {
            continue;
        };
        let header = RtpPacket::parse(packet)?.header;
        let arrived = clock + Duration::from_millis(record.t.saturating_sub(base_ms));
        let push = assembler.push_packet(header.ssrc, packet, arrived)?;
        if push.losses > 0 || push.dropped_timestamps {
            total_losses += push.losses as u64;
            if push.dropped_timestamps {
                dropped_ts += 1;
            }
            println!(
                "t={} PUSH losses={} dropped_ts={}",
                record.t, push.losses, push.dropped_timestamps
            );
        }
        let out = assembler.receive()?;
        ignored += out.ignored_units as u64;
        if out.chain_reset {
            resets += 1;
            println!("t={} CHAIN_RESET", record.t);
        }
        if out.stream_error {
            println!("t={} STREAM_ERROR", record.t);
        }
        if let Some(frame) = out.frame {
            frames += 1;
            if frame.access_units.len() != AVC_VIDEO_SLICE_COUNT {
                incomplete += 1;
                println!(
                    "t={} FRAME ts={} bands={} ({:?})",
                    record.t,
                    frame.timestamp,
                    frame.access_units.len(),
                    frame
                        .access_units
                        .iter()
                        .map(|(i, _)| *i)
                        .collect::<Vec<_>>()
                );
            }
        }
    }
    println!(
        "frames={frames} losses={total_losses} chain_resets={resets} dropped_ts={dropped_ts} ignored={ignored} incomplete_frames={incomplete}"
    );
    Ok(())
}
