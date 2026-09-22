#![cfg(any(target_os = "macos", target_os = "windows"))]
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

/// How many band-0 access units `ARD_PROBE_DUMP_ES` writes.
const ES_DUMP_FRAMES: usize = 90;

/// Development-only capture of one media stream at two points at once: the
/// compressed access units exactly as the decoder receives them, and the NV12
/// planes the decoder produced for each one.
///
/// `ARD_PROBE_DUMP_STREAM=<prefix>` writes `<prefix>.es` (Annex-B, access units
/// in decode order), `<prefix>.es.idx` (one line per submitted access unit),
/// `<prefix>.planes` (the decoder's own planes, luma then interleaved chroma,
/// tightly packed) and `<prefix>.planes.idx` (one line per decoded plane set).
/// The two indexes share the decoder's submission number, so an independent
/// decoder can be compared picture by picture against this client at exactly
/// the point where the artifact is claimable. Nothing is written without the
/// variable; `ARD_PROBE_NO_PLANES=1` keeps only the encoded stream and its
/// index, which is what a long take needs.
struct StreamDump {
    access_units: std::fs::File,
    access_index: std::io::BufWriter<std::fs::File>,
    /// Present unless `ARD_PROBE_NO_PLANES` is set: the planes run about a
    /// gigabyte per ten seconds of 4K, and a take that only needs the encoded
    /// stream and its index should not have to write them.
    planes: Option<std::fs::File>,
    plane_index: Option<std::io::BufWriter<std::fs::File>>,
    access_offset: u64,
    plane_offset: u64,
}

impl StreamDump {
    fn from_environment() -> io::Result<Option<Self>> {
        let Some(prefix) = env::var_os("ARD_PROBE_DUMP_STREAM").map(std::path::PathBuf::from)
        else {
            return Ok(None);
        };
        let open = |suffix: &str| -> io::Result<std::fs::File> {
            let mut path = prefix.clone().into_os_string();
            path.push(suffix);
            std::fs::File::create(std::path::PathBuf::from(path))
        };
        // The planes are about a gigabyte per ten seconds of 4K, so a long take
        // that only needs the encoded stream and its index can leave them out.
        let want_planes = env::var_os("ARD_PROBE_NO_PLANES").is_none();
        let planes = want_planes.then(|| open(".planes")).transpose()?;
        let plane_index = want_planes
            .then(|| open(".planes.idx").map(std::io::BufWriter::new))
            .transpose()?;
        Ok(Some(Self {
            access_units: open(".es")?,
            access_index: std::io::BufWriter::new(open(".es.idx")?),
            planes,
            plane_index,
            access_offset: 0,
            plane_offset: 0,
        }))
    }

    /// Record one access unit in submission order and return its ordinal, which
    /// is the decoder's submission number for this sample.
    fn push_access_unit(
        &mut self,
        ordinal: u64,
        slice_index: usize,
        timestamp: u32,
        unit: &ard_rs::media_stream::AccessUnit,
    ) -> io::Result<()> {
        use std::io::Write as _;
        let annex_b = unit.to_annex_b();
        self.access_units.write_all(&annex_b)?;
        writeln!(
            self.access_index,
            "{ordinal} {slice_index} {timestamp} {} {}",
            self.access_offset,
            annex_b.len(),
        )?;
        self.access_offset += annex_b.len() as u64;
        Ok(())
    }

    /// Record the planes the decoder produced for one submission.
    fn push_planes(&mut self, output: &media::DecodedOutput) -> io::Result<()> {
        use std::io::Write as _;
        let (width, height) = output
            .frame
            .as_ref()
            .map(|frame| (frame.width, frame.height))
            .unwrap_or((0, 0));
        let (Some(planes), Some(plane_index)) = (self.planes.as_mut(), self.plane_index.as_mut())
        else {
            return Ok(());
        };
        let mut length = 0_u64;
        if let Some(frame) = output.frame.as_ref() {
            planes.write_all(&frame.y_plane)?;
            planes.write_all(&frame.uv_plane)?;
            length = (frame.y_plane.len() + frame.uv_plane.len()) as u64;
        }
        writeln!(
            plane_index,
            "{} {} {} {width} {height} {} {length} {}",
            output.submission,
            output.stream_index,
            output.timestamp,
            self.plane_offset,
            output.status,
        )?;
        self.plane_offset += length;
        Ok(())
    }

    fn finish(&mut self) -> io::Result<()> {
        use std::io::Write as _;
        self.access_index.flush()?;
        if let Some(plane_index) = self.plane_index.as_mut() {
            plane_index.flush()?;
        }
        Ok(())
    }
}
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::{Duration, Instant};

use ard_rs::media_stream::UdpStreamKind;
use ard_rs::media_stream::udp::{AvcStreamCrypto, AvcVideoStreamReceiver};
use ard_rs::{
    ArdClient, ArdClientConfig, ArdClientEvent, ArdDisplayConfiguration, ArdVideoQuality,
    ArdVirtualDisplay, Framebuffer,
};
use ard_rs::{RawStreamKind, RawStreamSink, TakeOrigin};
#[cfg(target_os = "windows")]
use media::mft::MftDecoder as PlatformVideoDecoder;
use media::pipeline::{AvcReceiveEvent, AvcReceivePump, SliceCompositor};
#[cfg(target_os = "macos")]
use media::vt::VideoToolboxDecoder as PlatformVideoDecoder;
use sha2::{Digest, Sha256};

fn main() -> Result<(), Box<dyn Error>> {
    // A remote Mac left at its lock screen turns the display off after a short
    // idle period, after which the encoder streams an all-black framebuffer. A
    // modifier key press is harmless on the lock screen and resets the idle
    // timer, so a sequential native-then-viewer capture can still record a
    // meaningful reference. Set ARD_PROBE_NO_WAKE=1 to disable it.
    let wake_disabled = env::var_os("ARD_PROBE_NO_WAKE").is_some();
    // A pure observation run must not disturb the remote picture: the default
    // verification path clicks the remote desktop to measure input latency, and
    // a capture that has to keep the content bit-identical sets this instead.
    let input_disabled = env::var_os("ARD_PROBE_NO_INPUT").is_some();
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
        // The remaining profiles all run over the TCP/RFB path, which is the
        // only way to exercise MVS (encoding 1011) against a real server.
        // MVS (encoding 1011) is the first entry of the Adaptive profile; the
        // 1002 in the High profile is Apple's "thousands of colours", not MVS.
        Some("mvs") | Some("adaptive") => ArdVideoQuality::Adaptive,
        Some("high") => ArdVideoQuality::High,
        Some("medium") => ArdVideoQuality::Medium,
        Some("low") => ArdVideoQuality::Low,
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

    let mut config = ArdClientConfig::new(address.clone(), username.into_bytes(), password.clone());
    // The installed client keeps the RFB automatic-update loop running beside
    // the media stream. A take that has to see the media stream on its own
    // turns that loop off, which is what the native client does once its media
    // stream is up.
    if env::var_os("ARD_PROBE_NO_AUTO_UPDATES").is_some() {
        config.automatic_updates = false;
    }
    // The media-stream offer carries one frame-rate bit, so a take that has to
    // ask the server for something other than 60 fps sets the interval here.
    if let Some(millis) = env::var("ARD_PROBE_FRAME_INTERVAL_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
    {
        config.frame_interval = std::time::Duration::from_millis(millis);
    }
    password.fill(0);
    config.video_quality = quality;
    config.timeout = Duration::from_secs(10);
    config.display_configuration =
        fixed_size.map(|(width, height)| ArdDisplayConfiguration::single(width, height));
    let mut client = ArdClient::connect(config)?;
    let input = client.input();
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

    let started_at = Instant::now();
    let deadline = started_at + Duration::from_secs(max_seconds);
    if !quality.is_high_performance() {
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
            // A development switch that arms a raw-stream take for this
            // session, so the "take starts without a keyframe" path can be
            // exercised: `ARD_PROBE_RAW_DIR` plus `ARD_RECORD_RAW_STREAM=1`.
            let probe_sink = env::var_os("ARD_PROBE_RAW_DIR").and_then(|dir| {
                RawStreamSink::from_environment(std::path::PathBuf::from(dir), address.clone())
            });
            // Describe the stream before any take starts, exactly as the
            // viewer's pipeline does: a dump whose header does not name the
            // codec and frame size cannot be replayed.
            if let Some(sink) = probe_sink.as_ref() {
                sink.describe_stream(
                    RawStreamKind::Video,
                    ard_rs::MediaDetails {
                        codec: Some(match codec {
                            ard_rs::MediaStreamCodec::Hevc => "HEVC".to_owned(),
                            ard_rs::MediaStreamCodec::H264 => "H264".to_owned(),
                        }),
                        payload_type: Some(payload_type),
                        codec_type: None,
                        width: fixed_size.map(|(width, _)| width),
                        height: fixed_size.map(|(_, height)| height),
                    },
                );
            }
            let receiver = AvcVideoStreamReceiver::new(
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
                probe_sink.clone(),
            )?;
            key_blob.fill(0);
            feedback_key_blob.fill(0);
            let receive_pump = AvcReceivePump::spawn(receiver, Arc::new(AtomicBool::new(false)))
                .map_err(io::Error::other)?;
            // A remote Mac that is left at its lock screen turns the display off
            // after a short idle period. Software ARD sessions keep streaming the
            // resulting all-black framebuffer, so a sequential native-then-viewer
            // capture would otherwise record a black reference. A modifier
            // key press is harmless on the lock screen and resets the idle timer.
            let mut last_wake_at = None;
            if !wake_disabled {
                let _ = input.send_key_event(true, ard_rs::XK_SHIFT_LEFT);
                let _ = input.send_key_event(false, ard_rs::XK_SHIFT_LEFT);
                last_wake_at = Some(Instant::now());
                std::thread::sleep(Duration::from_millis(400));
            }
            let mut decoder = PlatformVideoDecoder::new(codec);
            let mut compositor = SliceCompositor::new(target_dimensions);
            let mut access_units = 0usize;
            let mut decoded_frames = 0usize;
            let mut composite_frames = 0usize;
            let mut changed_frames = 0usize;
            let mut previous_hash = None;
            let mut frame_accumulator = ProbeFrameAccumulator::default();
            let mut input_injected = false;
            let mut first_composite_at = None;
            let mut post_input_frames = 0usize;
            let mut max_non_black = 0usize;
            let mut packet_reassembly_latencies = Vec::new();
            let mut batch_holdbacks = Vec::new();
            let mut decode_latencies = Vec::new();
            let mut input_queued_at = None;
            let mut input_record_before = 0_u64;
            let mut input_write_completed_at = None;
            let mut response_first_packet_at = None;
            let mut response_decoded_at = None;
            // The receive pump drops its queue and asks for a fresh IRAP when the
            // consumer falls behind. Creating the VideoToolbox session can take
            // longer than the pump's 16-batch startup window, so one or more
            // resets right after the stream starts are expected and recoverable:
            // the next sync frame rebuilds the chain. Report them (they are a
            // latency/robustness data point) and keep going instead of failing
            // the whole capture.
            let mut resets: Vec<String> = Vec::new();
            // Optional Annex-B dump of band 0 for an independent decoder
            // cross-check. Analysis artifact only, written outside the repo.
            let es_dump = std::env::var_os("ARD_PROBE_DUMP_ES").map(std::path::PathBuf::from);
            let mut es_dump_frames = 0_usize;
            let mut stream_dump = StreamDump::from_environment()?;
            let mut stream_dump_ordinal = 0_u64;
            // Development switch: ask for a fresh keyframe N milliseconds in, so
            // a take can show whether the server answers a picture-loss
            // indication at all (`ARD_PROBE_KEYFRAME_AT_MS`).
            let keyframe_deadline = env::var("ARD_PROBE_KEYFRAME_AT_MS")
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok())
                .map(|after| started_at + Duration::from_millis(after));
            let mut keyframe_requested = false;
            let arm_deadline = env::var("ARD_PROBE_ARM_TAKE_AT_MS")
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok())
                .map(|after| started_at + Duration::from_millis(after));
            let mut take_armed = false;
            while Instant::now() < deadline {
                if let Some(when) = arm_deadline
                    && !take_armed
                    && Instant::now() >= when
                    && let Some(sink) = probe_sink.as_ref()
                {
                    take_armed = true;
                    let origin = TakeOrigin::new();
                    origin.set(Instant::now());
                    sink.start_recording(origin);
                    eprintln!(
                        "armed a raw-stream take {when:?} into the session: {}",
                        sink.raw_path(RawStreamKind::Server).display()
                    );
                }
                if let Some(when) = keyframe_deadline
                    && !keyframe_requested
                    && Instant::now() >= when
                {
                    keyframe_requested = true;
                    match receive_pump.request_keyframe() {
                        Ok(()) => println!("requested a keyframe {when:?} into the take"),
                        Err(error) => println!("keyframe request failed: {error}"),
                    }
                }
                let batch = match receive_pump.receive_timeout(Duration::from_millis(20)) {
                    Some(AvcReceiveEvent::Frame(batch)) => batch,
                    Some(AvcReceiveEvent::Reset(reason)) => {
                        resets.push(format!("{reason:?}"));
                        if resets.len() > 32 {
                            return Err(io::Error::other(format!(
                                "live verification prediction chain kept resetting: {resets:?}"
                            ))
                            .into());
                        }
                        continue;
                    }
                    Some(AvcReceiveEvent::Error(error)) => {
                        return Err(io::Error::other(error).into());
                    }
                    None => continue,
                };
                let timestamp = batch.timestamp;
                let first_packet_received_at = batch.first_packet_received_at;
                let first_access_unit_completed_at = batch.first_access_unit_completed_at;
                let batch_released_at = batch.released_at;
                packet_reassembly_latencies.push(
                    first_access_unit_completed_at
                        .saturating_duration_since(first_packet_received_at),
                );
                batch_holdbacks.push(
                    batch_released_at.saturating_duration_since(first_access_unit_completed_at),
                );
                let mut outputs = Vec::with_capacity(batch.access_units.len());
                for (slice_index, unit) in batch.access_units {
                    access_units += 1;
                    if let Some(dump) = stream_dump.as_mut() {
                        dump.push_access_unit(
                            stream_dump_ordinal,
                            slice_index,
                            unit.timestamp,
                            &unit,
                        )?;
                        stream_dump_ordinal += 1;
                    }
                    if let Some(path) = es_dump.as_ref()
                        && es_dump_frames < ES_DUMP_FRAMES
                    {
                        // Annex-B dump of every band in the order the decoder
                        // receives them, for an independent decoder cross-check
                        // (ffmpeg). The four bands are one serial prediction
                        // chain, so only the interleaved order is decodable.
                        // Analysis artifact only: it is written outside the
                        // repository and never committed.
                        if let Ok(mut file) = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(path)
                        {
                            use std::io::Write as _;
                            let _ = file.write_all(&unit.to_annex_b());
                        }
                        if slice_index == 0 {
                            es_dump_frames += 1;
                        }
                    }
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
                let decoded_at = Instant::now();
                decode_latencies.push(decoded_at.saturating_duration_since(batch_released_at));
                let current_input_metrics = input.metrics();
                if input_injected
                    && input_write_completed_at.is_none()
                    && current_input_metrics.user_input_records_written > input_record_before
                {
                    input_write_completed_at = current_input_metrics.last_user_input_completed_at;
                }
                let errors = decoder.take_errors();
                if !errors.is_empty() {
                    return Err(io::Error::other(errors.join("; ")).into());
                }
                if access_units <= 32 && !outputs.is_empty() {
                    println!(
                        "input timestamp={timestamp} produced output timestamps={:?}",
                        outputs
                            .iter()
                            .map(|output| output.timestamp)
                            .collect::<Vec<_>>()
                    );
                }
                for output_frame in &outputs {
                    if let Some(dump) = stream_dump.as_mut() {
                        dump.push_planes(output_frame)?;
                    }
                    if output_frame.status != 0 {
                        return Err(io::Error::other(format!(
                            "platform decoder callback failed: status={} flags={:#x}",
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
                    if decoded_frames <= 8 {
                        let hash = native_slice_hash(frame);
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
                    frame_accumulator
                        .apply(composite)
                        .map_err(io::Error::other)?;
                    if composite_frames == 1 {
                        frame_accumulator.report_sample_range();
                    }
                    // Track this unconditionally: it used to be updated only on
                    // the verification path, so a long run that never reached its
                    // frame target always reported `max_non_black=0` and could
                    // not tell a black screen from a working one.
                    max_non_black = max_non_black.max(frame_accumulator.non_black_pixels());
                    // The remote display turns off again after a few idle
                    // minutes, and the stream then carries black frames. A long
                    // stability run that only wakes once would spend most of its
                    // time measuring a black screen, so re-wake (rate limited)
                    // whenever the picture goes black.
                    if !wake_disabled
                        && frame_accumulator.non_black_pixels()
                            < frame_accumulator.pixel_count() / 100
                        && last_wake_at
                            .is_none_or(|at: Instant| at.elapsed() >= Duration::from_secs(20))
                    {
                        let _ = input.send_key_event(true, ard_rs::XK_SHIFT_LEFT);
                        let _ = input.send_key_event(false, ard_rs::XK_SHIFT_LEFT);
                        last_wake_at = Some(Instant::now());
                    }
                    let composite_hash = frame_accumulator.signature().ok_or_else(|| {
                        io::Error::other("full four-band frame is not initialized")
                    })?;
                    if !input_disabled && !input_injected && media_age >= Duration::from_secs(2) {
                        let click_x = u16::try_from(composite.width * 3 / 4).unwrap_or(u16::MAX);
                        let click_y = u16::try_from(composite.height / 2).unwrap_or(u16::MAX);
                        input_record_before = input.metrics().user_input_records_written;
                        input_write_completed_at = None;
                        input_queued_at = Some(Instant::now());
                        input.send_pointer_events(&[
                            (0, click_x, click_y),
                            (1, click_x, click_y),
                            (0, click_x, click_y),
                        ])?;
                        input_injected = true;
                        previous_hash = Some(composite_hash);
                        println!(
                            "injected one ordered remote pointer click after media settled; awaiting the first post-write changed frame"
                        );
                        continue;
                    }
                    if !input_injected {
                        previous_hash = Some(composite_hash);
                        continue;
                    }
                    post_input_frames += 1;
                    let changed = previous_hash.is_some_and(|previous| previous != composite_hash);
                    if changed {
                        changed_frames += 1;
                        if response_first_packet_at.is_none()
                            && input_write_completed_at
                                .is_some_and(|written| first_packet_received_at >= written)
                        {
                            response_first_packet_at = Some(first_packet_received_at);
                            response_decoded_at = Some(decoded_at);
                        }
                    }
                    previous_hash = Some(composite_hash);
                }
                if input_injected
                    && response_decoded_at.is_some()
                    && decoded_frames >= target_frames
                    && post_input_frames >= 4
                    && changed_frames > 0
                    && let Some(frame) = composite
                {
                    let non_black_y = frame_accumulator.non_black_pixels();
                    if non_black_y < frame_accumulator.pixel_count() / 100 {
                        continue;
                    }
                    let rgba = frame_accumulator.to_rgba();
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
                    let response = input_queued_at
                        .zip(input_write_completed_at)
                        .zip(response_first_packet_at.zip(response_decoded_at))
                        .map(|((queued, written), (received, decoded))| {
                            format!(
                                "input_queue_to_flush_ms={:.3} write_to_first_observed_changed_frame_packet_ms={:.3} write_to_first_observed_changed_frame_decode_ms={:.3} queue_to_first_observed_changed_frame_decode_ms={:.3}",
                                written.saturating_duration_since(queued).as_secs_f64() * 1_000.0,
                                received.saturating_duration_since(written).as_secs_f64() * 1_000.0,
                                decoded.saturating_duration_since(written).as_secs_f64() * 1_000.0,
                                decoded.saturating_duration_since(queued).as_secs_f64() * 1_000.0,
                            )
                        })
                        .unwrap_or_else(|| "observed changed-frame timing unavailable".to_owned());
                    let holdback = duration_percentiles(&mut batch_holdbacks);
                    let reassembly = duration_percentiles(&mut packet_reassembly_latencies);
                    let decode = duration_percentiles(&mut decode_latencies);
                    let stats = receive_pump.stats();
                    if stats.packet_losses != 0 {
                        return Err(io::Error::other(format!(
                            "live verification observed {} RTP access-unit losses",
                            stats.packet_losses,
                        ))
                        .into());
                    }
                    println!(
                        "verified: codec={codec:?} frames={decoded_frames} composites={composite_frames} changed={changed_frames} dimensions={}x{} RTP_reassembly_p50/p95_ms={:.3}/{:.3} DON_reorder_p50/p95_ms={:.3}/{:.3} release_to_decode_p50/p95_ms={:.3}/{:.3} {response} non_black={non_black} green_dominant={green_dominant} packets={} decrypted={} feedback={} resets={resets:?} output={output}",
                        frame.width,
                        frame.height,
                        reassembly.0,
                        reassembly.1,
                        holdback.0,
                        holdback.1,
                        decode.0,
                        decode.1,
                        stats.packets_received,
                        stats.decrypted_packets,
                        stats.heartbeats_sent,
                    );
                    write_png(
                        &output,
                        frame_accumulator.width,
                        frame_accumulator.height,
                        &rgba,
                    )?;
                    return Ok(());
                }
            }
            let stats = receive_pump.stats();
            return Err(io::Error::other(format!(
                "timed out after {decoded_frames}/{target_frames} decoded frames from {access_units} access units: composites={composite_frames} input_injected={input_injected} post_input={post_input_frames} changed={changed_frames} max_non_black={max_non_black} packets={} decrypted={} feedback={}",
                stats.packets_received,
                stats.decrypted_packets,
                stats.heartbeats_sent,
            ))
            .into());
        }
    }
    Err(io::Error::other("timed out waiting for AVC negotiation").into())
}

fn duration_percentiles(samples: &mut [Duration]) -> (f64, f64) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }
    samples.sort_unstable();
    let percentile = |numerator: usize, denominator: usize| {
        let index = (samples.len() - 1)
            .saturating_mul(numerator)
            .div_ceil(denominator);
        samples[index].as_secs_f64() * 1_000.0
    };
    (percentile(50, 100), percentile(95, 100))
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

#[derive(Default)]
struct ProbeFrameAccumulator {
    width: u32,
    height: u32,
    y_plane: Vec<u8>,
    uv_plane: Vec<u8>,
    range: Option<media::YuvRange>,
    matrix: Option<media::YuvMatrix>,
    primaries: Option<media::YuvPrimaries>,
    slice_versions: [Option<u64>; 4],
    next_version: u64,
}

impl ProbeFrameAccumulator {
    /// Report the sample range actually present in the decoded planes.
    ///
    /// This settles the limited-vs-full range question with evidence rather
    /// than from the pixel-format fourcc alone: a limited-range (studio-swing)
    /// stream keeps luma inside roughly 16..235 and chroma inside 16..240,
    /// while a full-range stream reaches 0 and 255.
    fn report_sample_range(&self) {
        if self.y_plane.is_empty() {
            return;
        }
        let luma_min = *self.y_plane.iter().min().expect("non-empty luma plane");
        let luma_max = *self.y_plane.iter().max().expect("non-empty luma plane");
        let luma_mean = self.y_plane.iter().map(|v| u64::from(*v)).sum::<u64>() as f64
            / self.y_plane.len() as f64;
        let chroma_min = *self.uv_plane.iter().min().expect("non-empty chroma plane");
        let chroma_max = *self.uv_plane.iter().max().expect("non-empty chroma plane");
        let chroma_mean = self.uv_plane.iter().map(|v| u64::from(*v)).sum::<u64>() as f64
            / self.uv_plane.len() as f64;
        println!(
            "sample range: luma min={luma_min} max={luma_max} mean={luma_mean:.1}  \
             chroma min={chroma_min} max={chroma_max} mean={chroma_mean:.1}  \
             declared_range={:?} matrix={:?} primaries={:?}",
            self.range, self.matrix, self.primaries,
        );
    }

    fn apply(&mut self, frame: &media::DecodedFrame) -> Result<(), String> {
        if self.width != frame.width
            || self.height != frame.height
            || self.range != Some(frame.range)
            || self.matrix != Some(frame.matrix)
            || self.primaries != Some(frame.primaries)
        {
            self.width = frame.width;
            self.height = frame.height;
            let width = frame.width as usize;
            let height = frame.height as usize;
            let uv_row_bytes = frame.width.div_ceil(2) as usize * 2;
            self.y_plane = vec![0; width.saturating_mul(height)];
            self.uv_plane =
                vec![128; uv_row_bytes.saturating_mul(frame.height.div_ceil(2) as usize)];
            self.range = Some(frame.range);
            self.matrix = Some(frame.matrix);
            self.primaries = Some(frame.primaries);
            self.slice_versions = [None; 4];
            self.next_version = 0;
        }

        let width = self.width as usize;
        let uv_row_bytes = self.width.div_ceil(2) as usize * 2;
        for update in &frame.updates {
            let y_bytes = width
                .checked_mul(update.y_rows as usize)
                .ok_or_else(|| "probe luma update size overflow".to_owned())?;
            let y_start = width
                .checked_mul(update.y_origin as usize)
                .ok_or_else(|| "probe luma update origin overflow".to_owned())?;
            let y_end = y_start
                .checked_add(y_bytes)
                .filter(|end| *end <= self.y_plane.len())
                .ok_or_else(|| "probe luma update exceeds the target frame".to_owned())?;
            if update.pixels.y_plane.len() < y_bytes {
                return Err("decoded luma slice is shorter than its visible rows".to_owned());
            }
            let visible_y = &update.pixels.y_plane[..y_bytes];
            let luma_changed = self.y_plane[y_start..y_end] != *visible_y;

            let uv_bytes = uv_row_bytes
                .checked_mul(update.uv_rows as usize)
                .ok_or_else(|| "probe chroma update size overflow".to_owned())?;
            let uv_start = uv_row_bytes
                .checked_mul(update.uv_origin as usize)
                .ok_or_else(|| "probe chroma update origin overflow".to_owned())?;
            let uv_end = uv_start
                .checked_add(uv_bytes)
                .filter(|end| *end <= self.uv_plane.len())
                .ok_or_else(|| "probe chroma update exceeds the target frame".to_owned())?;
            if update.pixels.uv_plane.len() < uv_bytes {
                return Err("decoded chroma slice is shorter than its visible rows".to_owned());
            }
            let visible_uv = &update.pixels.uv_plane[..uv_bytes];
            let chroma_changed = self.uv_plane[uv_start..uv_end] != *visible_uv;
            if luma_changed {
                self.y_plane[y_start..y_end].copy_from_slice(visible_y);
            }
            if chroma_changed {
                self.uv_plane[uv_start..uv_end].copy_from_slice(visible_uv);
            }
            let slot = self
                .slice_versions
                .get_mut(update.slice_index)
                .ok_or_else(|| {
                    "decoded slice index exceeds the native four-band layout".to_owned()
                })?;
            if slot.is_none() || luma_changed || chroma_changed {
                self.next_version = self.next_version.wrapping_add(1);
                *slot = Some(self.next_version);
            }
        }
        Ok(())
    }

    fn signature(&self) -> Option<[u64; 4]> {
        let mut signature = [0; 4];
        for (index, version) in self.slice_versions.iter().enumerate() {
            signature[index] = (*version)?;
        }
        Some(signature)
    }

    fn pixel_count(&self) -> usize {
        self.y_plane.len()
    }

    fn non_black_pixels(&self) -> usize {
        self.y_plane.iter().filter(|sample| **sample > 24).count()
    }

    /// Diagnostic-only conversion for the probe's final PNG and pixel
    /// statistics. The live viewer uploads these same persistent planes
    /// directly to its NV12 GPU shader.
    fn to_rgba(&self) -> Vec<u8> {
        let (kr, kb) = match self.matrix.expect("initialized probe frame matrix") {
            media::YuvMatrix::Bt601 => (0.299_f32, 0.114_f32),
            media::YuvMatrix::Bt709 => (0.2126_f32, 0.0722_f32),
            media::YuvMatrix::Bt2020 => (0.2627_f32, 0.0593_f32),
        };
        let kg = 1.0 - kr - kb;
        let (y_scale, chroma_scale, y_offset) =
            match self.range.expect("initialized probe frame range") {
                media::YuvRange::Video => (255.0 / 219.0, 255.0 / 224.0, 16.0 / 255.0),
                media::YuvRange::Full => (1.0, 1.0, 0.0),
            };
        let width = self.width as usize;
        let height = self.height as usize;
        let uv_row_bytes = self.width.div_ceil(2) as usize * 2;
        let mut rgba = Vec::with_capacity(width * height * 4);
        for row in 0..height {
            for column in 0..width {
                let y = f32::from(self.y_plane[row * width + column]) / 255.0;
                let uv = row / 2 * uv_row_bytes + column / 2 * 2;
                let cb = f32::from(self.uv_plane[uv]) / 255.0 - 128.0 / 255.0;
                let cr = f32::from(self.uv_plane[uv + 1]) / 255.0 - 128.0 / 255.0;
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
    let mut metadata_only_frames = 0usize;
    // The remote display sleeps after a few idle minutes and the resulting
    // framebuffer is black; wake it the same way the media-stream path does.
    if std::env::var_os("ARD_PROBE_NO_WAKE").is_none() {
        let _ = client.send_key_event(true, ard_rs::XK_SHIFT_LEFT);
        let _ = client.send_key_event(false, ard_rs::XK_SHIFT_LEFT);
        thread::sleep(Duration::from_millis(400));
    }
    while Instant::now() < deadline {
        let event = client.next_event()?;
        if !matches!(event, ArdClientEvent::Frame(_)) {
            if frames == 0 && changed < 8 {
                changed += 1;
                eprintln!("conventional probe: server event before any frame: {event:?}");
            }
            continue;
        }
        frames += 1;
        if !framebuffer_to_rgba(client.framebuffer(), &mut rgba) {
            // The GPU MVS profiles keep pixels as tiles rather than in the CPU
            // framebuffer, so this diagnostic cannot render them. Counting the
            // frame is still the answer to "does the server deliver image
            // rectangles at all", which is what debugging the RFB path needs.
            if frames == 1 {
                println!(
                    "conventional probe: frame 1 arrived but has no CPU framebuffer \
                     (GPU tile path); counting frames only"
                );
            }
            metadata_only_frames += 1;
            continue;
        }
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
    if metadata_only_frames != 0 {
        println!(
            "conventional probe: {metadata_only_frames} image frame(s) arrived but the framebuffer \
             is GPU-tile backed, so no pixels could be checked"
        );
        return Ok(());
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
