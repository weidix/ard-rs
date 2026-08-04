#![forbid(unsafe_code)]

//! Small headless live-session probe for comparing decoder resource usage.

use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::time::Instant;

use ard_rs::{ArdClient, ArdClientConfig, ArdVideoQuality, MvsGpuTile};

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = env::args().skip(1);
    let address = args.next().ok_or_else(usage)?;
    let username = args.next().ok_or_else(usage)?;
    let frame_count = args
        .next()
        .unwrap_or_else(|| "1".to_owned())
        .parse::<usize>()
        .map_err(|_| usage())?;
    let quality = match args.next().as_deref() {
        None | Some("adaptive") => ArdVideoQuality::Adaptive,
        Some("low") => ArdVideoQuality::Low,
        Some("medium") => ArdVideoQuality::Medium,
        Some("high") => ArdVideoQuality::High,
        Some("full") => ArdVideoQuality::Full,
        Some(_) => return Err(usage().into()),
    };
    if frame_count == 0 || args.next().is_some() {
        return Err(usage().into());
    }

    let password = env::var_os("ARD_PASSWORD")
        .ok_or_else(|| std::io::Error::other("ARD_PASSWORD is not set"))?
        .to_string_lossy()
        .into_owned()
        .into_bytes();
    let mut config = ArdClientConfig::new(address, username.into_bytes(), password);
    config.video_quality = quality;

    let started = Instant::now();
    let mut client = ArdClient::connect(config)?;
    let connected = started.elapsed();
    let mut updates = 0_usize;
    let mut rectangles = 0_usize;
    let mut wire_bytes = 0_usize;
    let mut payload_bytes = 0_usize;
    let mut gpu_frames = 0_usize;
    let mut gpu_tiles = 0_usize;
    let mut gpu_changed_tiles = 0_usize;
    let mut previous_tiles = HashMap::<(u16, u16), (u8, u8, MvsGpuTile)>::new();

    for _ in 0..frame_count {
        let info = client.next_frame()?;
        updates = updates.saturating_add(info.framebuffer_updates);
        rectangles = rectangles.saturating_add(info.rectangle_count);
        wire_bytes = wire_bytes.saturating_add(info.wire_bytes);
        payload_bytes = payload_bytes.saturating_add(info.payload_bytes);
        client.drain_gpu_mvs_frames(|frame| {
            gpu_frames = gpu_frames.saturating_add(1);
            gpu_tiles = gpu_tiles.saturating_add(frame.tiles.len());
            for update in frame.tiles {
                let key = (update.x, update.y);
                let changed = previous_tiles.get(&key).is_none_or(|previous| {
                    previous.0 != update.width
                        || previous.1 != update.height
                        || previous.2 != update.tile
                });
                if changed {
                    gpu_changed_tiles = gpu_changed_tiles.saturating_add(1);
                }
                previous_tiles.insert(key, (update.width, update.height, update.tile));
            }
        });
    }

    let elapsed = started.elapsed();
    println!(
        "quality={} frames={} updates={} rectangles={} gpu_frames={} gpu_tiles={} gpu_changed_tiles={} wire_bytes={} payload_bytes={} connect_ms={} total_ms={}",
        quality.label(),
        frame_count,
        updates,
        rectangles,
        gpu_frames,
        gpu_tiles,
        gpu_changed_tiles,
        wire_bytes,
        payload_bytes,
        connected.as_secs_f64() * 1000.0,
        elapsed.as_secs_f64() * 1000.0,
    );
    Ok(())
}

fn usage() -> std::io::Error {
    std::io::Error::other(
        "usage: live_metrics ADDRESS USERNAME [FRAME_COUNT] [adaptive|low|medium|high|full]",
    )
}
