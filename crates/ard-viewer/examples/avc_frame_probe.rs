#![cfg(target_os = "macos")]
#![allow(dead_code)]

#[path = "../src/config.rs"]
mod config;
#[path = "../src/i18n.rs"]
mod i18n;
#[path = "../src/icons.rs"]
mod icons;
#[allow(unused_imports)]
#[path = "../src/media/mod.rs"]
mod media;
#[path = "../src/state.rs"]
mod state;

use std::env;
use std::error::Error;
use std::fs::File;
use std::io::{self, BufRead, Write};
use std::time::{Duration, Instant};

use ard_rs::media_stream::UdpStreamKind;
use ard_rs::media_stream::udp::{AvcStreamCrypto, AvcVideoStreamReceiver};
use ard_rs::{ArdClient, ArdClientConfig, ArdClientEvent, ArdVideoQuality};
use media::pipeline::SliceCompositor;
use media::vt::VideoToolboxDecoder;
use sha2::{Digest, Sha256};

fn main() -> Result<(), Box<dyn Error>> {
    let address = env::args().nth(1).ok_or_else(|| {
        io::Error::other(
            "usage: avc_frame_probe ADDRESS USERNAME OUTPUT.ppm [MAX_SECONDS] [hevc|avc] [FRAME_COUNT]",
        )
    })?;
    let username = env::args().nth(2).ok_or_else(|| {
        io::Error::other(
            "usage: avc_frame_probe ADDRESS USERNAME OUTPUT.ppm [MAX_SECONDS] [hevc|avc] [FRAME_COUNT]",
        )
    })?;
    let output = env::args().nth(3).ok_or_else(|| {
        io::Error::other(
            "usage: avc_frame_probe ADDRESS USERNAME OUTPUT.ppm [MAX_SECONDS] [hevc|avc] [FRAME_COUNT]",
        )
    })?;
    let max_seconds = env::args()
        .nth(4)
        .map(|value| value.parse::<u64>())
        .transpose()?
        .unwrap_or(45);
    let quality = match env::args().nth(5).as_deref() {
        None | Some("hevc") => ArdVideoQuality::HighPerformanceHevc,
        Some("avc") => ArdVideoQuality::HighPerformanceAvc,
        Some(codec) => return Err(io::Error::other(format!("unsupported codec: {codec}")).into()),
    };
    let target_frames = env::args()
        .nth(6)
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(60)
        .max(2);

    let mut password = Vec::new();
    io::stdin().lock().read_until(b'\n', &mut password)?;
    while password
        .last()
        .is_some_and(|byte| byte.is_ascii_whitespace())
    {
        password.pop();
    }
    if password.is_empty() {
        password = config::load_password(&address, &username)
            .ok_or_else(|| {
                io::Error::other("no password on stdin or in the saved credential vault")
            })?
            .into_bytes();
    }

    let mut config = ArdClientConfig::new(address, username.into_bytes(), password.clone());
    password.fill(0);
    config.video_quality = quality;
    config.timeout = Duration::from_secs(10);
    let mut client = ArdClient::connect(config)?;
    println!(
        "connected: {} framebuffer={}x{}",
        client.server_name(),
        client.framebuffer().width(),
        client.framebuffer().height(),
    );
    let target_dimensions = (
        u32::from(client.framebuffer().width()),
        u32::from(client.framebuffer().height()),
    );

    let deadline = Instant::now() + Duration::from_secs(max_seconds);
    while Instant::now() < deadline {
        if let ArdClientEvent::MediaStream(media) = client.next_event()? {
            let (
                endpoints,
                mut key_blob,
                mut feedback_key_blob,
                codec,
                payload_type,
                remote_ssrc,
                local_ssrc,
            ) = media.into_video_pipeline_parts();
            let mut receiver = AvcVideoStreamReceiver::new(
                &endpoints,
                UdpStreamKind::Video1,
                AvcStreamCrypto {
                    server_to_viewer_key_blob: &key_blob,
                    viewer_to_server_key_blob: &feedback_key_blob,
                    remote_ssrc,
                    local_ssrc,
                },
                codec,
                payload_type,
            )?;
            key_blob.fill(0);
            feedback_key_blob.fill(0);
            let mut decoder = VideoToolboxDecoder::new(codec);
            let mut compositor = SliceCompositor::new(target_dimensions);
            let mut access_units = 0usize;
            let mut decoded_frames = 0usize;
            let mut composite_frames = 0usize;
            let mut changed_frames = 0usize;
            let mut previous_hash = None;
            while Instant::now() < deadline {
                if let Ok(Some((slice_index, unit))) = receiver.receive() {
                    access_units += 1;
                    let nal_types: Vec<_> = unit
                        .nal_units
                        .iter()
                        .filter_map(|nal| {
                            nal.first().map(|byte| match codec {
                                ard_rs::media_stream::MediaStreamCodec::H264 => byte & 0x1f,
                                ard_rs::media_stream::MediaStreamCodec::Hevc => (byte >> 1) & 0x3f,
                            })
                        })
                        .collect();
                    let unit_timestamp = unit.timestamp;
                    let nal_sizes = unit.nal_units.iter().map(Vec::len).collect::<Vec<_>>();
                    let prefixes = unit
                        .nal_units
                        .iter()
                        .map(|nal| nal[..nal.len().min(32)].to_vec())
                        .collect::<Vec<_>>();
                    let encoded_bytes = unit.avcc_len();
                    let decoded = decoder.decode(&unit);
                    if decoded.is_none() {
                        if access_units <= 8 {
                            println!(
                                "undecoded access unit {access_units}: slice={slice_index} timestamp={} types={nal_types:?} sizes={:?} prefixes={prefixes:02x?}",
                                unit_timestamp, nal_sizes,
                            );
                        }
                        continue;
                    }
                    let decoded = decoded.expect("checked above");
                    for (decoded_slice_index, frame) in [(slice_index, decoded)] {
                        decoded_frames += 1;
                        let hash: [u8; 32] = Sha256::digest(&frame.rgba).into();
                        if decoded_frames <= 8 {
                            println!(
                                "decoded frame {decoded_frames}: slice={} timestamp={} types={nal_types:?} bytes={} dimensions={}x{} hash={:02x?}",
                                decoded_slice_index,
                                unit_timestamp,
                                encoded_bytes,
                                frame.width,
                                frame.height,
                                &hash[..4],
                            );
                        }
                        let composite = compositor.push(decoded_slice_index, encoded_bytes, frame);
                        if let Some(composite) = &composite {
                            composite_frames += 1;
                            let composite_hash: [u8; 32] = Sha256::digest(&composite.rgba).into();
                            if previous_hash.is_some_and(|previous| previous != composite_hash) {
                                changed_frames += 1;
                            }
                            previous_hash = Some(composite_hash);
                            if composite_frames == 1 {
                                client.send_pointer_event(0, 24, 16)?;
                                client.send_pointer_event(1, 24, 16)?;
                                client.send_pointer_event(0, 24, 16)?;
                                client.send_key_event(true, 0x20)?;
                                client.send_key_event(false, 0x20)?;
                                println!(
                                    "injected remote pointer click and Space after first composite"
                                );
                            }
                        }
                        if decoded_frames == target_frames {
                            let frame = composite.ok_or_else(|| {
                                io::Error::other(
                                    "target frame count ended before a complete composite frame",
                                )
                            })?;
                            let non_black = frame
                                .rgba
                                .chunks_exact(4)
                                .filter(|pixel| pixel[..3].iter().any(|channel| *channel > 8))
                                .count();
                            let green_dominant = frame
                                .rgba
                                .chunks_exact(4)
                                .filter(|pixel| {
                                    pixel[1] > pixel[0].saturating_add(32)
                                        && pixel[1] > pixel[2].saturating_add(32)
                                })
                                .count();
                            println!(
                                "verified: codec={codec:?} frames={decoded_frames} composites={composite_frames} changed={changed_frames} dimensions={}x{} non_black={non_black} green_dominant={green_dominant} packets={} decrypted={} feedback={} output={output}",
                                frame.width,
                                frame.height,
                                receiver.packets_received(),
                                receiver.decrypted_packets(),
                                receiver.heartbeats_sent(),
                            );
                            write_ppm(&output, frame.width, frame.height, &frame.rgba)?;
                            return Ok(());
                        }
                    }
                }
            }
            return Err(io::Error::other(format!(
                "timed out after {decoded_frames}/{target_frames} decoded frames from {access_units} access units: packets={} decrypted={} feedback={}",
                receiver.packets_received(),
                receiver.decrypted_packets(),
                receiver.heartbeats_sent(),
            ))
            .into());
        }
    }
    Err(io::Error::other("timed out waiting for AVC negotiation").into())
}

fn write_ppm(path: &str, width: u32, height: u32, rgba: &[u8]) -> io::Result<()> {
    let pixel_count = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .ok_or_else(|| io::Error::other("decoded dimensions overflow"))?;
    if rgba.len() != pixel_count.saturating_mul(4) {
        return Err(io::Error::other("decoded RGBA length mismatch"));
    }
    let mut file = File::create(path)?;
    write!(file, "P6\n{width} {height}\n255\n")?;
    for pixel in rgba.chunks_exact(4) {
        file.write_all(&pixel[..3])?;
    }
    Ok(())
}
