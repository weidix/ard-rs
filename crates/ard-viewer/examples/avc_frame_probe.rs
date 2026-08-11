#![cfg(target_os = "macos")]

#[allow(unused_imports)]
#[path = "../src/media/mod.rs"]
mod media;

use std::env;
use std::error::Error;
use std::fs::File;
use std::io::{self, BufRead, Write};
use std::time::{Duration, Instant};

use ard_rs::media_stream::UdpStreamKind;
use ard_rs::media_stream::udp::AvcVideoStreamReceiver;
use ard_rs::{ArdClient, ArdClientConfig, ArdClientEvent, ArdVideoQuality};
use media::vt::VideoToolboxDecoder;

fn main() -> Result<(), Box<dyn Error>> {
    let address = env::args().nth(1).ok_or_else(|| {
        io::Error::other(
            "usage: avc_frame_probe ADDRESS USERNAME OUTPUT.ppm [MAX_SECONDS] [hevc|avc]",
        )
    })?;
    let username = env::args().nth(2).ok_or_else(|| {
        io::Error::other(
            "usage: avc_frame_probe ADDRESS USERNAME OUTPUT.ppm [MAX_SECONDS] [hevc|avc]",
        )
    })?;
    let output = env::args().nth(3).ok_or_else(|| {
        io::Error::other(
            "usage: avc_frame_probe ADDRESS USERNAME OUTPUT.ppm [MAX_SECONDS] [hevc|avc]",
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

    let mut password = Vec::new();
    io::stdin().lock().read_until(b'\n', &mut password)?;
    while password
        .last()
        .is_some_and(|byte| byte.is_ascii_whitespace())
    {
        password.pop();
    }

    let mut config = ArdClientConfig::new(address, username.into_bytes(), password.clone());
    password.fill(0);
    config.video_quality = quality;
    config.timeout = Duration::from_secs(10);
    let mut client = ArdClient::connect(config)?;
    println!("connected: {}", client.server_name());

    let deadline = Instant::now() + Duration::from_secs(max_seconds);
    while Instant::now() < deadline {
        if let ArdClientEvent::MediaStream(media) = client.next_event()? {
            let (endpoints, mut key_blob, codec, payload_type, remote_ssrc) =
                media.into_video_pipeline_parts();
            let mut receiver = AvcVideoStreamReceiver::new(
                &endpoints,
                UdpStreamKind::Video1,
                &key_blob,
                codec,
                payload_type,
                remote_ssrc,
            )?;
            key_blob.fill(0);
            let mut decoder = VideoToolboxDecoder::new(codec);
            while Instant::now() < deadline {
                match receiver.receive() {
                    Ok(Some(unit)) => {
                        if let Some(frame) = decoder.decode(&unit) {
                            write_ppm(&output, frame.width, frame.height, &frame.rgba)?;
                            println!(
                                "decoded frame: codec={codec:?} {}x{} output={output}",
                                frame.width, frame.height
                            );
                            return Ok(());
                        }
                    }
                    Ok(None) | Err(_) => {}
                }
            }
            return Err(io::Error::other("timed out waiting for decoded AVC frame").into());
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
