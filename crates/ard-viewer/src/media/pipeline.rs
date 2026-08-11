//! Live decode pipeline: UDP/SRTP/RTP receive (ard-core) -> VideoToolbox ->
//! RGBA callback.

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ard_rs::media_stream::udp::AvcVideoStreamReceiver;
use ard_rs::media_stream::{MediaStreamCodec, MediaUdpEndpoints, UdpStreamKind, VideoCodecConfig};

use super::DecodedFrame;
use super::vt::VideoToolboxDecoder;

/// Spawn the AVC video receive/decode loop for one stream.
///
/// The loop blocks on UDP, decrypts SRTP, reassembles RTP into access units
/// (all inside ard-core), decodes with VideoToolbox and invokes `on_frame`
/// with each displayable RGBA frame. It stops when `stop` is set or the
/// remote end stays silent for `idle_timeout`.
pub fn spawn_avc_video_pipeline(
    endpoints: MediaUdpEndpoints,
    key_blob: Vec<u8>,
    codec: MediaStreamCodec,
    payload_type: u8,
    remote_ssrc: u32,
    stop: Arc<AtomicBool>,
    on_frame: impl FnMut(DecodedFrame) + Send + 'static,
) -> JoinHandle<()> {
    let codec_config = VideoCodecConfig {
        codec: Some(codec),
        payload_type: Some(payload_type),
        ..VideoCodecConfig::default()
    };
    spawn_avc_video_pipeline_with_config(
        endpoints,
        key_blob,
        codec_config,
        remote_ssrc,
        stop,
        on_frame,
    )
}

/// Spawn the AVC pipeline using the complete codec configuration extracted
/// from the negotiator answer. In particular, the worker never infers a
/// codec or payload type from RTP payload bytes.
pub fn spawn_avc_video_pipeline_with_config(
    endpoints: MediaUdpEndpoints,
    key_blob: Vec<u8>,
    codec_config: VideoCodecConfig,
    remote_ssrc: u32,
    stop: Arc<AtomicBool>,
    mut on_frame: impl FnMut(DecodedFrame) + Send + 'static,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("ard-media-stream".into())
        .spawn(move || {
            let mut key_blob = key_blob;
            let Some(codec) = codec_config.codec else {
                key_blob.fill(0);
                return;
            };
            let Some(payload_type) = codec_config.payload_type else {
                key_blob.fill(0);
                return;
            };
            let mut receiver = match AvcVideoStreamReceiver::new(
                &endpoints,
                UdpStreamKind::Video1,
                &key_blob,
                codec,
                payload_type,
                remote_ssrc,
            ) {
                Ok(receiver) => receiver,
                Err(_) => {
                    key_blob.fill(0);
                    return;
                }
            };
            // SrtpContext keeps only the derived session material. Do not
            // retain the negotiated master blob for the lifetime of the
            // receive/decode thread.
            key_blob.fill(0);
            let mut decoder = VideoToolboxDecoder::new(codec);
            let mut last_frame = Instant::now();
            const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
            while !stop.load(Ordering::Relaxed) {
                match receiver.receive() {
                    Ok(Some(unit)) => {
                        let encoded_bytes = unit.avcc_len();
                        if let Some(mut frame) = decoder.decode(&unit) {
                            last_frame = Instant::now();
                            frame.encoded_bytes = encoded_bytes;
                            on_frame(frame);
                        }
                    }
                    Ok(None) => {}
                    Err(_) => {
                        if last_frame.elapsed() > IDLE_TIMEOUT {
                            break;
                        }
                    }
                }
            }
        })
        .expect("AVC media pipeline thread should start")
}
