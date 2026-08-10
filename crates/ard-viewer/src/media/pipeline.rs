//! Live decode pipeline: UDP/SRTP/RTP receive (ard-core) -> VideoToolbox ->
//! RGBA callback.

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ard_rs::avc::udp::AvcVideoStreamReceiver;
use ard_rs::avc::{MediaStreamCodec, MediaUdpEndpoints, UdpStreamKind};

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
    stop: Arc<AtomicBool>,
    mut on_frame: impl FnMut(DecodedFrame) + Send + 'static,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("ard-avc-media".into())
        .spawn(move || {
            let mut receiver = match AvcVideoStreamReceiver::new(
                &endpoints,
                UdpStreamKind::Video1,
                &key_blob,
                codec,
            ) {
                Ok(receiver) => receiver,
                Err(_) => return,
            };
            let mut decoder = VideoToolboxDecoder::new(codec);
            let mut last_frame = Instant::now();
            const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
            while !stop.load(Ordering::Relaxed) {
                match receiver.receive() {
                    Ok(Some(unit)) => {
                        last_frame = Instant::now();
                        let encoded_bytes = unit.avcc_len();
                        if let Some(mut frame) = decoder.decode(&unit) {
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
