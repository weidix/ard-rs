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
use ard_rs::media_stream::{AccessUnit, MediaStreamCodec};
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
};
fn output(outputs: Vec<media::DecodedOutput>) {
    for o in outputs {
        if let Some(f) = o.frame {
            let mut hashes = [0xcbf29ce484222325u64; 2];
            for (plane, hash) in [&f.y_plane, &f.uv_plane].into_iter().zip(&mut hashes) {
                for &b in plane {
                    *hash = (*hash ^ u64::from(b)).wrapping_mul(0x100000001b3);
                }
            }
            let w = f.width as usize;
            println!(
                "{},{},{},{},{},{},{},{},{},{:?},{:?}",
                o.submission,
                o.stream_index,
                o.status,
                f.width,
                f.height,
                hashes[0],
                hashes[1],
                f.y_plane[100 * w + 100],
                f.uv_plane[50 * w + 100],
                f.range,
                f.primaries
            );
        } else {
            println!("{},{},{},no-image", o.submission, o.stream_index, o.status);
        }
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let prefix = std::env::args().nth(1).ok_or("prefix required")?;
    let mut file = fs::File::open(format!("{prefix}.es"))?;
    let idx = fs::read_to_string(format!("{prefix}.es.idx"))?;
    let mut decoder = media::vt::VideoToolboxDecoder::new(MediaStreamCodec::Hevc);
    let mut timestamp = None;
    for line in idx.lines() {
        let n = line
            .split_whitespace()
            .map(str::parse::<u64>)
            .collect::<Result<Vec<_>, _>>()?;
        if timestamp != Some(n[2]) {
            output(decoder.finish_frame());
            timestamp = Some(n[2]);
        }
        let mut data = vec![0; n[4] as usize];
        file.seek(SeekFrom::Start(n[3]))?;
        file.read_exact(&mut data)?;
        let mut starts = Vec::new();
        let mut p = 0;
        while p + 4 <= data.len() {
            if data[p..p + 4] == [0, 0, 0, 1] {
                starts.push(p);
                p += 4;
            } else {
                p += 1;
            }
        }
        let nals = starts
            .iter()
            .enumerate()
            .map(|(i, &s)| data[s + 4..starts.get(i + 1).copied().unwrap_or(data.len())].to_vec())
            .collect();
        output(decoder.decode(
            n[1] as usize,
            &AccessUnit {
                timestamp: n[2] as u32,
                decode_order_number: None,
                nal_units: nals,
            },
        ));
        for e in decoder.take_errors() {
            eprintln!("{e}");
        }
    }
    output(decoder.finish_frame());
    Ok(())
}
