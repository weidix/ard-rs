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
use std::io::{self, BufRead};
use std::thread;
use std::time::{Duration, Instant};

use ard_rs::media_stream::UdpStreamKind;
use ard_rs::media_stream::udp::{AvcStreamCrypto, AvcVideoStreamReceiver};
use ard_rs::{
    ArdClient, ArdClientConfig, ArdClientEvent, ArdDisplayConfiguration, ArdVideoQuality,
    ArdVirtualDisplay, Framebuffer,
};
use media::pipeline::SliceCompositor;
use media::vt::VideoToolboxDecoder;
use sha2::{Digest, Sha256};

fn main() -> Result<(), Box<dyn Error>> {
    let address = env::args().nth(1).ok_or_else(|| {
        io::Error::other(
            "usage: avc_nv12_frame_probe ADDRESS USERNAME OUTPUT.png [MAX_SECONDS] [hevc|avc|full] [FRAME_COUNT] [FIXED_SIZE]",
        )
    })?;
    let username = env::args().nth(2).ok_or_else(|| {
        io::Error::other(
            "usage: avc_nv12_frame_probe ADDRESS USERNAME OUTPUT.png [MAX_SECONDS] [hevc|avc|full] [FRAME_COUNT] [FIXED_SIZE]",
        )
    })?;
    let output = env::args().nth(3).ok_or_else(|| {
        io::Error::other(
            "usage: avc_nv12_frame_probe ADDRESS USERNAME OUTPUT.png [MAX_SECONDS] [hevc|avc|full] [FRAME_COUNT] [FIXED_SIZE]",
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
        Some("full") => ArdVideoQuality::Full,
        Some(codec) => return Err(io::Error::other(format!("unsupported codec: {codec}")).into()),
    };
    let target_frames = env::args()
        .nth(6)
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(60)
        .max(2);
    let fixed_size = env::args()
        .nth(7)
        .map(|value| {
            let (width, height) = value
                .split_once(['x', 'X'])
                .ok_or_else(|| io::Error::other("FIXED_SIZE must be WIDTHxHEIGHT"))?;
            let width = width.parse::<u32>().map_err(io::Error::other)?;
            let height = height.parse::<u32>().map_err(io::Error::other)?;
            if !ArdVirtualDisplay::is_supported_size(width, height) {
                return Err(io::Error::other(format!(
                    "unsupported fixed size {width}x{height}"
                )));
            }
            Ok::<_, io::Error>((width, height))
        })
        .transpose()?;

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
    config.display_configuration =
        fixed_size.map(|(width, height)| ArdDisplayConfiguration::single(width, height));
    let mut client = ArdClient::connect(config)?;
    println!(
        "connected: {} framebuffer={}x{}",
        client.server_name(),
        client.framebuffer().width(),
        client.framebuffer().height(),
    );
    let target_dimensions = fixed_size
        .map(|(width, height)| (width * 2, height * 2))
        .unwrap_or_else(|| {
            (
                u32::from(client.framebuffer().width()),
                u32::from(client.framebuffer().height()),
            )
        });

    let deadline = Instant::now() + Duration::from_secs(max_seconds);
    if quality == ArdVideoQuality::Full {
        return probe_full_framebuffer(
            &mut client,
            deadline,
            &output,
            target_frames,
            target_dimensions,
        );
    }
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
            ) = (*media).into_video_pipeline_parts();
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
            let mut input_injected = false;
            let mut first_composite_at = None;
            let mut post_input_frames = 0usize;
            let mut max_non_black = 0usize;
            while Instant::now() < deadline {
                let Some(batch) = receiver.receive_frame()? else {
                    continue;
                };
                let timestamp = batch.timestamp;
                let mut outputs = Vec::with_capacity(batch.access_units.len());
                for (slice_index, unit) in batch.access_units {
                    access_units += 1;
                    let decoded = decoder.decode(slice_index, &unit);
                    if decoded.is_empty() && access_units <= 8 {
                        let nal_types = unit
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
                            .collect::<Vec<_>>();
                        println!(
                            "submitted access unit {access_units}: slice={slice_index} timestamp={} types={nal_types:?}",
                            unit.timestamp,
                        );
                    }
                    outputs.extend(decoded);
                }
                outputs.extend(decoder.finish_frame());
                let errors = decoder.take_errors();
                if !errors.is_empty() {
                    return Err(io::Error::other(errors.join("; ")).into());
                }
                for output_frame in &outputs {
                    if output_frame.timestamp != timestamp {
                        return Err(io::Error::other(format!(
                            "VCP callback crossed RTP frame boundary: expected={timestamp} actual={}",
                            output_frame.timestamp
                        ))
                        .into());
                    }
                    if output_frame.status != 0 {
                        return Err(io::Error::other(format!(
                            "VCP callback failed: status={} flags={:#x}",
                            output_frame.status, output_frame.info_flags
                        ))
                        .into());
                    }
                    if let Some(error) = &output_frame.conversion_error {
                        return Err(io::Error::other(error.clone()).into());
                    }
                    let frame = output_frame.frame.as_ref();
                    let Some(frame) = frame else {
                        if access_units <= 8 {
                            println!(
                                "decode callback without image: slice={} timestamp={} submission={} status={} flags={:#x}",
                                output_frame.stream_index,
                                output_frame.timestamp,
                                output_frame.submission,
                                output_frame.status,
                                output_frame.info_flags,
                            );
                        }
                        continue;
                    };
                    decoded_frames += 1;
                    let hash = native_slice_hash(frame);
                    if decoded_frames <= 8 {
                        println!(
                            "decoded frame {decoded_frames}: slice={} timestamp={} submission={} bytes={} dimensions={}x{} hash={:02x?}",
                            output_frame.stream_index,
                            output_frame.timestamp,
                            output_frame.submission,
                            output_frame.encoded_bytes,
                            frame.width,
                            frame.height,
                            &hash[..4],
                        );
                    }
                    compositor
                        .push(
                            output_frame.stream_index,
                            output_frame.encoded_bytes,
                            Some(frame.clone()),
                        )
                        .map_err(io::Error::other)?;
                }
                let composite = compositor.finish_frame().map_err(io::Error::other)?;
                if let Some(composite) = &composite {
                    composite_frames += 1;
                    let media_age = first_composite_at
                        .get_or_insert_with(Instant::now)
                        .elapsed();
                    let (composite_y, composite_uv) = compose_native_frame(composite);
                    let composite_hash = native_planes_hash(&composite_y, &composite_uv);
                    if !input_injected && media_age >= Duration::from_secs(2) {
                        let click_x = u16::try_from(composite.width / 2).unwrap_or(u16::MAX);
                        let click_y = u16::try_from(composite.height / 2).unwrap_or(u16::MAX);
                        client.send_pointer_event(0, click_x, click_y)?;
                        client.send_pointer_event(1, click_x, click_y)?;
                        thread::sleep(Duration::from_millis(75));
                        client.send_pointer_event(0, click_x, click_y)?;
                        input_injected = true;
                        previous_hash = Some(composite_hash);
                        println!("injected one remote pointer click after media settled");
                        continue;
                    }
                    let pixel_count = composite_y.len();
                    let non_black = composite_y.iter().filter(|sample| **sample > 24).count();
                    max_non_black = max_non_black.max(non_black);
                    if non_black < pixel_count / 100 {
                        continue;
                    }
                    post_input_frames += 1;
                    if previous_hash.is_some_and(|previous| previous != composite_hash) {
                        changed_frames += 1;
                    }
                    previous_hash = Some(composite_hash);
                }
                if decoded_frames >= target_frames
                    && post_input_frames >= 4
                    && changed_frames > 0
                    && let Some(frame) = composite
                {
                    let rgba = native_frame_to_rgba(&frame);
                    let non_black = rgba
                        .chunks_exact(4)
                        .filter(|pixel| pixel[..3].iter().any(|channel| *channel > 8))
                        .count();
                    let green_dominant = rgba
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
                    write_png(&output, frame.width, frame.height, &rgba)?;
                    return Ok(());
                }
            }
            return Err(io::Error::other(format!(
                "timed out after {decoded_frames}/{target_frames} decoded frames from {access_units} access units: composites={composite_frames} input_injected={input_injected} post_input={post_input_frames} changed={changed_frames} max_non_black={max_non_black} packets={} decrypted={} feedback={}",
                receiver.packets_received(),
                receiver.decrypted_packets(),
                receiver.heartbeats_sent(),
            ))
            .into());
        }
    }
    Err(io::Error::other("timed out waiting for AVC negotiation").into())
}

fn native_frame_hash(frame: &media::DecodedFrame) -> [u8; 32] {
    let mut digest = Sha256::new();
    for update in &frame.updates {
        digest.update(&update.pixels.y_plane);
        digest.update(&update.pixels.uv_plane);
    }
    digest.finalize().into()
}

fn native_slice_hash(frame: &media::DecodedSlice) -> [u8; 32] {
    native_planes_hash(&frame.y_plane, &frame.uv_plane)
}

fn native_planes_hash(y_plane: &[u8], uv_plane: &[u8]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(y_plane);
    digest.update(uv_plane);
    digest.finalize().into()
}

fn compose_native_frame(frame: &media::DecodedFrame) -> (Vec<u8>, Vec<u8>) {
    let width = frame.width as usize;
    let height = frame.height as usize;
    let uv_row_bytes = frame.width.div_ceil(2) as usize * 2;
    let mut y_plane = vec![0; width * height];
    let mut uv_plane = vec![128; uv_row_bytes * frame.height.div_ceil(2) as usize];
    for update in &frame.updates {
        let y_bytes = width * update.y_rows as usize;
        let y_start = width * update.y_origin as usize;
        y_plane[y_start..y_start + y_bytes].copy_from_slice(&update.pixels.y_plane[..y_bytes]);
        let uv_bytes = uv_row_bytes * update.uv_rows as usize;
        let uv_start = uv_row_bytes * update.uv_origin as usize;
        uv_plane[uv_start..uv_start + uv_bytes]
            .copy_from_slice(&update.pixels.uv_plane[..uv_bytes]);
    }
    (y_plane, uv_plane)
}

/// Diagnostic-only conversion for the probe's PNG and pixel statistics. The
/// live viewer uploads these same planes directly to its NV12 GPU shader.
fn native_frame_to_rgba(frame: &media::DecodedFrame) -> Vec<u8> {
    let (kr, kb) = match frame.matrix {
        media::YuvMatrix::Bt601 => (0.299_f32, 0.114_f32),
        media::YuvMatrix::Bt709 => (0.2126_f32, 0.0722_f32),
        media::YuvMatrix::Bt2020 => (0.2627_f32, 0.0593_f32),
    };
    let kg = 1.0 - kr - kb;
    let (y_scale, chroma_scale, y_offset) = match frame.range {
        media::YuvRange::Video => (255.0 / 219.0, 255.0 / 224.0, 16.0 / 255.0),
        media::YuvRange::Full => (1.0, 1.0, 0.0),
    };
    let width = frame.width as usize;
    let height = frame.height as usize;
    let uv_row_bytes = frame.width.div_ceil(2) as usize * 2;
    let (y_plane, uv_plane) = compose_native_frame(frame);
    let mut rgba = Vec::with_capacity(width * height * 4);
    for row in 0..height {
        for column in 0..width {
            let y = f32::from(y_plane[row * width + column]) / 255.0;
            let uv = row / 2 * uv_row_bytes + column / 2 * 2;
            let cb = f32::from(uv_plane[uv]) / 255.0 - 128.0 / 255.0;
            let cr = f32::from(uv_plane[uv + 1]) / 255.0 - 128.0 / 255.0;
            let y = y_scale * (y - y_offset);
            let cb = cb * chroma_scale;
            let cr = cr * chroma_scale;
            let red = y + 2.0 * (1.0 - kr) * cr;
            let blue = y + 2.0 * (1.0 - kb) * cb;
            let green = y - 2.0 * kb * (1.0 - kb) / kg * cb - 2.0 * kr * (1.0 - kr) / kg * cr;
            rgba.extend(
                [red, green, blue].map(|value| (value.clamp(0.0, 1.0) * 255.0).round() as u8),
            );
            rgba.push(255);
        }
    }
    rgba
}

fn probe_full_framebuffer(
    client: &mut ArdClient,
    deadline: Instant,
    output: &str,
    target_frames: usize,
    target_dimensions: (u32, u32),
) -> Result<(), Box<dyn Error>> {
    let mut frames = 0usize;
    let mut changed = 0usize;
    let mut previous_hash = None;
    let mut input_injected = false;
    let mut rgba = Vec::new();
    let mut max_non_black = 0usize;
    while Instant::now() < deadline {
        if !matches!(client.next_event()?, ArdClientEvent::Frame(_)) {
            continue;
        }
        if !framebuffer_to_rgba(client.framebuffer(), &mut rgba) {
            return Err(io::Error::other("unsupported conventional framebuffer format").into());
        }
        frames += 1;
        let hash: [u8; 32] = Sha256::digest(&rgba).into();
        let non_black = rgba
            .chunks_exact(4)
            .filter(|pixel| pixel[..3].iter().any(|channel| *channel > 8))
            .count();
        max_non_black = max_non_black.max(non_black);
        if !input_injected {
            let click_x = u16::try_from(target_dimensions.0 / 2).unwrap_or(u16::MAX);
            let click_y = u16::try_from(target_dimensions.1 / 2).unwrap_or(u16::MAX);
            client.send_pointer_event(0, click_x, click_y)?;
            client.send_pointer_event(1, click_x, click_y)?;
            thread::sleep(Duration::from_millis(75));
            client.send_pointer_event(0, click_x, click_y)?;
            input_injected = true;
            previous_hash = Some(hash);
            println!("injected one remote pointer click in conventional probe");
            continue;
        }
        if previous_hash.is_some_and(|previous| previous != hash) {
            changed += 1;
        }
        previous_hash = Some(hash);
        let pixel_count = rgba.len() / 4;
        if frames >= target_frames && changed > 0 && non_black >= pixel_count / 100 {
            write_png(output, target_dimensions.0, target_dimensions.1, &rgba)?;
            println!(
                "verified conventional framebuffer: frames={frames} changed={changed} non_black={non_black} output={output}"
            );
            return Ok(());
        }
    }
    Err(io::Error::other(format!(
        "conventional probe timed out: frames={frames} changed={changed} input_injected={input_injected} max_non_black={max_non_black}"
    ))
    .into())
}

fn framebuffer_to_rgba(framebuffer: &Framebuffer, output: &mut Vec<u8>) -> bool {
    let format = framebuffer.pixel_format();
    let Ok(bytes_per_pixel) = format.bytes_per_pixel() else {
        return false;
    };
    let Some(pixel_count) =
        usize::from(framebuffer.width()).checked_mul(usize::from(framebuffer.height()))
    else {
        return false;
    };
    if framebuffer.pixels().len() != pixel_count.saturating_mul(bytes_per_pixel)
        || !format.true_color
        || format.red_max == 0
        || format.green_max == 0
        || format.blue_max == 0
    {
        return false;
    }
    output.clear();
    output.reserve(pixel_count.saturating_mul(4));
    for bytes in framebuffer.pixels().chunks_exact(bytes_per_pixel) {
        let value = match (bytes_per_pixel, format.big_endian) {
            (1, _) => u32::from(bytes[0]),
            (2, true) => u32::from(u16::from_be_bytes([bytes[0], bytes[1]])),
            (2, false) => u32::from(u16::from_le_bytes([bytes[0], bytes[1]])),
            (4, true) => u32::from_be_bytes(bytes.try_into().expect("pixel width checked")),
            (4, false) => u32::from_le_bytes(bytes.try_into().expect("pixel width checked")),
            _ => return false,
        };
        output.extend_from_slice(&[
            scale_pixel_channel(value, format.red_shift, format.red_max),
            scale_pixel_channel(value, format.green_shift, format.green_max),
            scale_pixel_channel(value, format.blue_shift, format.blue_max),
            255,
        ]);
    }
    true
}

fn scale_pixel_channel(value: u32, shift: u8, max: u16) -> u8 {
    ((((value >> shift) & u32::from(max)) * 255 + u32::from(max) / 2) / u32::from(max)) as u8
}

fn write_png(path: &str, width: u32, height: u32, rgba: &[u8]) -> io::Result<()> {
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
    let file = File::create(path)?;
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(io::Error::other)?;
    writer.write_image_data(rgba).map_err(io::Error::other)
}
