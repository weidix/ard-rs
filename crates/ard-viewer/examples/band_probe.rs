#![cfg(target_os = "macos")]
#![allow(dead_code, unused_imports)]

#[path = "../src/config.rs"]
mod config;
#[path = "../src/i18n.rs"]
mod i18n;
#[path = "../src/icons.rs"]
mod icons;
#[path = "../src/media/mod.rs"]
mod media;
#[path = "../src/state.rs"]
mod state;

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ard_rs::RawStreamIndex;
use ard_rs::media_stream::{MediaStreamCodec, RtpPacket, VideoStreamAssembler};

/// Replay a decrypted media-stream dump through the production RTP assembler
/// and the production platform decoder, writing both ends out for comparison:
///
/// - `<out>/es/band<N>.h265`  Annex-B access units, in decode order, per band
/// - `<out>/es.idx`           one line per access unit: `ts slot don types...`
/// - `<out>/dec/band<N>.bin`  the decoder's own plane bytes, tightly packed
/// - `<out>/dec.idx`          `submission slot ts width height status bytes range matrix primaries`
///
/// Usage: `cargo run --release -p ard-viewer --example band_probe -- DUMP.jsonl OUTDIR`
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let index_path = PathBuf::from(args.next().ok_or("usage: band_probe DUMP.jsonl OUTDIR")?);
    let out_dir = PathBuf::from(args.next().ok_or("usage: band_probe DUMP.jsonl OUTDIR")?);

    let index = RawStreamIndex::read(&index_path)?;
    let raw = fs::read(RawStreamIndex::raw_path(&index_path))?;
    println!(
        "dump: {} records, take {:?} ms",
        index.records.len(),
        index.take_ms
    );

    let codec = if index.header.contains("HEVC") {
        MediaStreamCodec::Hevc
    } else {
        MediaStreamCodec::H264
    };

    let ssrcos: Vec<u32> = index
        .records
        .iter()
        .filter_map(|record| record.ssrc)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    println!("bands (ascending SSRC): {ssrcos:?}");
    let mut assembler = VideoStreamAssembler::new(codec);
    for ssrc in &ssrcos {
        assembler.expect_stream(*ssrc);
    }

    fs::create_dir_all(out_dir.join("es"))?;
    fs::create_dir_all(out_dir.join("dec"))?;
    let mut es_files: Vec<BufWriter<File>> = (0..ssrcos.len())
        .map(|band| {
            BufWriter::new(
                File::create(out_dir.join(format!("es/band{band}.h265"))).expect("create es"),
            )
        })
        .collect();
    let mut es_index = BufWriter::new(File::create(out_dir.join("es.idx"))?);
    let mut dec_files: Vec<BufWriter<File>> = (0..ssrcos.len())
        .map(|band| {
            BufWriter::new(
                File::create(out_dir.join(format!("dec/band{band}.bin"))).expect("create dec"),
            )
        })
        .collect();
    let mut dec_index = BufWriter::new(File::create(out_dir.join("dec.idx"))?);

    let mut decoder = media::vt::VideoToolboxDecoder::new(codec);
    let clock = Instant::now();
    let base_ms = index.records.first().map(|record| record.t).unwrap_or(0);
    let mut frames = 0u64;
    let mut units = 0u64;
    let mut plane_rows = 0u64;
    let mut plane_cursors = vec![0u64; ssrcos.len()];

    for (number, record) in index.records.iter().enumerate() {
        let start = record.offset as usize;
        let end = start
            .checked_add(record.length as usize)
            .ok_or("record out of range")?;
        let packet = raw.get(start..end).ok_or("record out of range")?;
        let header = RtpPacket::parse(packet)
            .map_err(|error| format!("record {number} is not RTP: {error}"))?
            .header;
        let arrived = clock + Duration::from_millis(record.t.saturating_sub(base_ms));
        assembler.push_packet(header.ssrc, packet, arrived)?;
        let assembled = assembler.receive()?;
        let Some(frame) = assembled.frame else {
            continue;
        };
        frames += 1;
        let mut decoded = Vec::new();
        for (slot, unit) in &frame.access_units {
            units += 1;
            let types: Vec<String> = unit
                .nal_units
                .iter()
                .map(|nal| match codec {
                    MediaStreamCodec::Hevc => format!("{}", (nal[0] >> 1) & 0x3f),
                    MediaStreamCodec::H264 => format!("{}", nal[0] & 0x1f),
                })
                .collect();
            writeln!(
                es_index,
                "{} {} {} {} {} {}",
                frame.timestamp,
                slot,
                unit.decode_order_number.unwrap_or(0),
                unit.nal_units.len(),
                unit.nal_units.iter().map(|nal| nal.len()).sum::<usize>(),
                types.join(","),
            )?;
            let output = &mut es_files[*slot];
            for nal in &unit.nal_units {
                output.write_all(&[0, 0, 0, 1])?;
                output.write_all(nal)?;
            }
            decoded.extend(decoder.decode(*slot, unit));
        }
        decoded.extend(decoder.finish_frame());
        for error in decoder.take_errors() {
            eprintln!("decode error: {error}");
        }
        for output in decoded {
            let Some(slice) = output.frame else { continue };
            plane_rows += 1;
            let bytes = &mut dec_files[output.stream_index];
            let offset = plane_cursors[output.stream_index];
            bytes.write_all(&slice.y_plane)?;
            bytes.write_all(&slice.uv_plane)?;
            let length = (slice.y_plane.len() + slice.uv_plane.len()) as u64;
            plane_cursors[output.stream_index] += length;
            writeln!(
                dec_index,
                "{} {} {} {} {} {} {} {} {:?} {:?} {:?}",
                output.submission,
                output.stream_index,
                output.timestamp,
                slice.width,
                slice.height,
                output.status,
                offset,
                length,
                slice.range,
                slice.matrix,
                slice.primaries,
            )?;
        }
    }
    for file in es_files.iter_mut().chain(dec_files.iter_mut()) {
        file.flush()?;
    }
    es_index.flush()?;
    dec_index.flush()?;
    println!("frames={frames} units={units} plane_sets={plane_rows}");
    Ok(())
}
