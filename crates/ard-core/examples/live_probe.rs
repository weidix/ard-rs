#![forbid(unsafe_code)]

//! Live protocol probe against a real Apple Screen Sharing server.
//!
//! Answers the two layout questions the disassembly alone could not settle:
//!
//! 1. the DisplayInfo2 (`1105`) rectangle axis order, by dumping the raw
//!    payload bytes the server actually sends;
//! 2. whether Apple attaches a DONL to HEVC single-NAL RTP units, which
//!    RFC 7798 does not define.
//!
//! Usage:
//!
//! ```sh
//! ARD_TRACE_DISPLAY_INFO2=1 ARD_RTP_WIRE_TRACE=1 \
//!   cargo run -p ard-core --release --example live_probe -- \
//!   127.0.0.1:5900 wei adaptive 1920x1080 15
//! ```
//!
//! Arguments: `ADDRESS USERNAME QUALITY [WIDTHxHEIGHT] [SECONDS]`.
//! The password is read from stdin and is never printed or written to disk.

use std::error::Error as StdError;
use std::io::{self, BufRead, Write};
use std::time::{Duration, Instant};

use ard_rs::{
    ArdClient, ArdClientConfig, ArdClientEvent, ArdDisplayConfiguration, ArdVideoQuality,
};

fn quality_from_name(name: &str) -> Option<ArdVideoQuality> {
    match name {
        "adaptive" => Some(ArdVideoQuality::Adaptive),
        "hevc" | "high-performance" => Some(ArdVideoQuality::HighPerformanceHevc),
        "avc" => Some(ArdVideoQuality::HighPerformanceAvc),
        "full" => Some(ArdVideoQuality::Full),
        "low" => Some(ArdVideoQuality::Low),
        "medium" => Some(ArdVideoQuality::Medium),
        "high" => Some(ArdVideoQuality::High),
        _ => None,
    }
}

fn display_from_name(name: &str) -> Option<ArdDisplayConfiguration> {
    let (width, height) = name.split_once('x')?;
    Some(ArdDisplayConfiguration::single(
        width.trim().parse().ok()?,
        height.trim().parse().ok()?,
    ))
}

fn main() -> Result<(), Box<dyn StdError>> {
    let mut arguments = std::env::args().skip(1);
    let address = arguments
        .next()
        .unwrap_or_else(|| "127.0.0.1:5900".to_owned());
    let username = arguments.next().unwrap_or_else(|| "wei".to_owned());
    let quality_name = arguments.next().unwrap_or_else(|| "adaptive".to_owned());
    let display_name = arguments.next();
    let seconds: u64 = arguments
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(15);

    let quality = quality_from_name(&quality_name)
        .ok_or_else(|| format!("unknown quality {quality_name}"))?;
    let display = display_name
        .as_deref()
        .map(|name| display_from_name(name).ok_or_else(|| format!("bad size {name}")))
        .transpose()?;

    eprint!("password for {username}@{address}: ");
    io::stderr().flush()?;
    let mut password = String::new();
    io::stdin().lock().read_line(&mut password)?;
    let password = password.trim_end_matches(['\r', '\n']).to_owned();
    if password.is_empty() {
        return Err("empty password".into());
    }

    let mut config = ArdClientConfig::new(
        address.clone(),
        username.into_bytes(),
        password.into_bytes(),
    );
    config.video_quality = quality;
    config.display_configuration = display.clone();
    config.timeout = Duration::from_secs(10);

    eprintln!(
        "probe: connecting to {address} quality={quality_name} display={:?}",
        display_name
    );
    let mut client = ArdClient::connect(config)?;
    eprintln!("probe: connected to {:?}", client.server_name());

    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut frames = 0_usize;
    let mut media_streams = 0_usize;
    while Instant::now() < deadline {
        match client.next_event() {
            Ok(ArdClientEvent::Frame(info)) => {
                frames += 1;
                if frames <= 3 || frames.is_multiple_of(30) {
                    eprintln!(
                        "probe: frame #{} updates={} rects={} payload={}B framebuffer={}x{}",
                        info.index,
                        info.framebuffer_updates,
                        info.rectangle_count,
                        info.payload_bytes,
                        client.framebuffer().width(),
                        client.framebuffer().height(),
                    );
                }
            }
            Ok(ArdClientEvent::MediaStream(_)) => {
                media_streams += 1;
                eprintln!("probe: media stream negotiated (#{media_streams})");
            }
            Ok(ArdClientEvent::Clipboard(text)) => {
                eprintln!("probe: clipboard {}B", text.len());
            }
            Ok(ArdClientEvent::Bell | ArdClientEvent::StateChange) => {}
            Ok(ArdClientEvent::Reconnected) => eprintln!("probe: reconnected"),
            Err(error) => {
                eprintln!("probe: event error after {frames} frames: {error}");
                break;
            }
        }
    }

    match client.display_layout() {
        Some(layout) => eprintln!(
            "probe: display layout {}x{} with {} display(s): {:#?}",
            layout.framebuffer_width,
            layout.framebuffer_height,
            layout.displays.len(),
            layout.displays,
        ),
        None => eprintln!("probe: server sent no DisplayInfo2 layout"),
    }
    eprintln!("probe: done, {frames} frames, {media_streams} media stream(s)");
    Ok(())
}
