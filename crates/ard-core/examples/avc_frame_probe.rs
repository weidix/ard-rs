#![forbid(unsafe_code)]

use std::env;
use std::error::Error;
use std::io::{self, BufRead};
use std::time::{Duration, Instant};

use ard_rs::media_stream::UdpStreamKind;
use ard_rs::media_stream::udp::{AvcStreamCrypto, AvcVideoStreamReceiver};
use ard_rs::{ArdClient, ArdClientConfig, ArdClientEvent, ArdVideoQuality};

fn main() -> Result<(), Box<dyn Error>> {
    let address = env::args().nth(1).ok_or_else(|| {
        io::Error::other("usage: avc_frame_probe ADDRESS USERNAME [MAX_SECONDS] [hevc|avc]")
    })?;
    let username = env::args().nth(2).ok_or_else(|| {
        io::Error::other("usage: avc_frame_probe ADDRESS USERNAME [MAX_SECONDS] [hevc|avc]")
    })?;
    let max_seconds = env::args()
        .nth(3)
        .map(|value| value.parse::<u64>())
        .transpose()?
        .unwrap_or(30);
    let quality = match env::args().nth(4).as_deref() {
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
        match client.next_event()? {
            ArdClientEvent::MediaStream(media) => {
                println!("negotiated: {media:?}");
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
                let mut receive_errors = 0_usize;
                while Instant::now() < deadline {
                    match receiver.receive() {
                        Ok(Some((slice_index, unit))) => {
                            let nal_types: Vec<_> = unit
                                .nal_units
                                .iter()
                                .filter_map(|nal| {
                                    nal.first().map(|byte| match codec {
                                        ard_rs::media_stream::MediaStreamCodec::H264 => byte & 0x1f,
                                        ard_rs::media_stream::MediaStreamCodec::Hevc => {
                                            (byte >> 1) & 0x3f
                                        }
                                    })
                                })
                                .collect();
                            println!(
                                "frame: codec={codec:?} slice={slice_index} nal_units={} types={nal_types:?} bytes={} packets={} decrypted={} heartbeats={} receive_errors={receive_errors}",
                                unit.nal_units.len(),
                                unit.avcc_len(),
                                receiver.packets_received(),
                                receiver.decrypted_packets(),
                                receiver.heartbeats_sent(),
                            );
                            return Ok(());
                        }
                        Ok(None) => {}
                        Err(error) => {
                            receive_errors += 1;
                            if receive_errors <= 5 {
                                eprintln!("receive error: {error}");
                            }
                        }
                    }
                }
                return Err(io::Error::other(format!(
                    "timed out waiting for AVC frame: packets={} decrypted={} heartbeats={}",
                    receiver.packets_received(),
                    receiver.decrypted_packets(),
                    receiver.heartbeats_sent(),
                ))
                .into());
            }
            ArdClientEvent::Frame(info) => println!(
                "rfb frame: updates={} rectangles={} bytes={}",
                info.framebuffer_updates, info.rectangle_count, info.payload_bytes
            ),
            ArdClientEvent::Clipboard(_)
            | ArdClientEvent::Bell
            | ArdClientEvent::StateChange
            | ArdClientEvent::Reconnected => {}
        }
    }
    Err(io::Error::other("timed out waiting for AVC negotiation").into())
}
