#![forbid(unsafe_code)]

//! One-shot AVC media stream capture oracle.
//!
//! Completes the RFB 003.889 handshake exactly like `mvs_oracle_server.rs`,
//! then records every byte the client sends. When the client emits the
//! media-stream configuration message (`0x1c`), the oracle:
//!
//! * dumps the raw message to `avc_offer.bin` for wire-format confirmation;
//! * replies with server message `0x23` (`RFBMediaStreamMessage1`) carrying
//!   the AVC encoding marker and three consecutive UDP ports;
//! * binds the UDP ports and records the first datagrams per port, so the
//!   RTP/SRTP framing can be confirmed against a real client.
//!
//! The reply envelope is best-effort (the exact wrapper is the unknown under
//! investigation); every message received afterwards is logged so the session
//! can be replayed and the envelope refined.

use std::env;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};

use ard_rs::avc::{
    ENCODING_AVC_MEDIA_STREAM, MediaStreamMessage1, SERVER_MEDIA_STREAM_MESSAGE_TYPE,
};

const DH_GROUP2_PRIME: [u8; 128] = [
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xc9, 0x0f, 0xda, 0xa2, 0x21, 0x68, 0xc2, 0x34,
    0xc4, 0xc6, 0x62, 0x8b, 0x80, 0xdc, 0x1c, 0xd1, 0x29, 0x02, 0x4e, 0x08, 0x8a, 0x67, 0xcc, 0x74,
    0x02, 0x0b, 0xbe, 0xa6, 0x3b, 0x13, 0x9b, 0x22, 0x51, 0x4a, 0x08, 0x79, 0x8e, 0x34, 0x04, 0xdd,
    0xef, 0x95, 0x19, 0xb3, 0xcd, 0x3a, 0x43, 0x1b, 0x30, 0x2b, 0x0a, 0x6d, 0xf2, 0x5f, 0x14, 0x37,
    0x4f, 0xe1, 0x35, 0x6d, 0x6d, 0x51, 0xc2, 0x45, 0xe4, 0x85, 0xb5, 0x76, 0x62, 0x5e, 0x7e, 0xc6,
    0xf4, 0x4c, 0x42, 0xe9, 0xa6, 0x37, 0xed, 0x6b, 0x0b, 0xff, 0x5c, 0xb6, 0xf4, 0x06, 0xb7, 0xed,
    0xee, 0x38, 0x6b, 0xfb, 0x5a, 0x89, 0x9f, 0xa5, 0xae, 0x9f, 0x24, 0x11, 0x7c, 0x4b, 0x1f, 0xe6,
    0x49, 0x28, 0x66, 0x51, 0xec, 0xe6, 0x53, 0x81, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
];
const CLIENT_MEDIA_STREAM: u8 = 0x1c;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn log(writer: &mut BufWriter<File>, text: &str) -> std::io::Result<()> {
    writeln!(writer, "{text}")?;
    writer.flush()
}

fn ard_send_challenge(stream: &mut (impl Read + Write)) -> std::io::Result<()> {
    stream.write_all(&[0, 2])?; // two-byte big-endian generator
    stream.write_all(&(DH_GROUP2_PRIME.len() as u16).to_be_bytes())?;
    stream.write_all(&DH_GROUP2_PRIME)?;
    let mut public_key = [0_u8; 128];
    public_key[127] = 2; // g^1 mod p
    stream.write_all(&public_key)?;
    stream.flush()?;
    println!("sent ARD Diffie-Hellman parameters");
    Ok(())
}

fn vnc_authenticate(stream: &mut (impl Read + Write)) -> std::io::Result<()> {
    stream.write_all(&[0x5a; 16])?;
    stream.flush()?;
    let mut response = [0_u8; 16];
    stream.read_exact(&mut response)?;
    println!("received {}-byte Apple VNC response", response.len());
    Ok(())
}

fn send_raw_rectangle(stream: &mut TcpStream) -> std::io::Result<()> {
    // Keep the RFB session healthy with an 8x8 black Raw rectangle.
    let mut update = vec![0, 0, 0, 1]; // FramebufferUpdate, one rectangle
    update.extend_from_slice(&0u16.to_be_bytes());
    update.extend_from_slice(&0u16.to_be_bytes());
    update.extend_from_slice(&8u16.to_be_bytes());
    update.extend_from_slice(&8u16.to_be_bytes());
    update.extend_from_slice(&0i32.to_be_bytes()); // Raw
    update.extend_from_slice(&[0; 8 * 8 * 4]);
    stream.write_all(&update)?;
    stream.flush()
}

#[allow(dead_code)]
fn read_exact_capture(
    stream: &mut TcpStream,
    capture: &mut BufWriter<File>,
    len: usize,
) -> std::io::Result<Vec<u8>> {
    let mut bytes = vec![0_u8; len];
    stream.read_exact(&mut bytes)?;
    capture.write_all(&bytes)?;
    capture.flush()?;
    Ok(bytes)
}

#[allow(dead_code)]
fn read_message(
    stream: &mut TcpStream,
    capture: &mut BufWriter<File>,
    out_dir: &str,
) -> std::io::Result<Option<Vec<u8>>> {
    let kind = read_exact_capture(stream, capture, 1)?[0];
    match kind {
        0 => {
            let body = read_exact_capture(stream, capture, 19)?;
            println!("SetPixelFormat");
            let _ = body;
        }
        2 => {
            let header = read_exact_capture(stream, capture, 3)?;
            let count = usize::from(u16::from_be_bytes([header[1], header[2]]));
            let encodings_raw = read_exact_capture(stream, capture, count.saturating_mul(4))?;
            let encodings: Vec<i32> = encodings_raw
                .chunks_exact(4)
                .map(|b| i32::from_be_bytes(b.try_into().unwrap()))
                .collect();
            let avc = encodings.contains(&ENCODING_AVC_MEDIA_STREAM);
            let mvs = encodings.contains(&1011);
            println!(
                "SetEncodings ({count}): AVC1010={avc} MVS1011={mvs} {:?}",
                encodings
            );
        }
        3 => {
            let body = read_exact_capture(stream, capture, 9)?;
            let _ = body;
            println!("FramebufferUpdateRequest");
        }
        4 => {
            read_exact_capture(stream, capture, 7)?;
            println!("KeyEvent");
        }
        5 => {
            read_exact_capture(stream, capture, 5)?;
            println!("PointerEvent");
        }
        6 => {
            let header = read_exact_capture(stream, capture, 7)?;
            let length = u32::from_be_bytes(header[3..7].try_into().unwrap()) as usize;
            read_exact_capture(stream, capture, length)?;
            println!("ClientCutText ({length} bytes)");
        }
        9 => {
            // ARD frame-buffer update request variant (16 bytes total).
            read_exact_capture(stream, capture, 15)?;
            println!("ARD message 0x09 (update request 2?)");
        }
        10 => {
            let body = read_exact_capture(stream, capture, 3)?;
            println!("Apple session options {:02x?}", body);
        }
        CLIENT_MEDIA_STREAM => {
            // ARD custom message framing: [type][pad][u16 BE length][payload].
            let prefix = read_exact_capture(stream, capture, 3)?;
            let length = u16::from_be_bytes([prefix[1], prefix[2]]);
            let body = read_exact_capture(stream, capture, usize::from(length))?;
            let full = [&[kind][..], &prefix, &body].concat();
            let file = format!("{out_dir}/avc_offer.bin");
            std::fs::write(&file, &full).expect("write offer");
            println!(
                ">>> MEDIA STREAM CONFIGURATION (0x1c), {} bytes -> {file}",
                full.len()
            );
            println!(">>> offer: {}", hex(&full[..full.len().min(160)]));
            return Ok(Some(full));
        }
        0x21 => {
            let prefix = read_exact_capture(stream, capture, 3)?;
            let length = u16::from_be_bytes([prefix[1], prefix[2]]);
            read_exact_capture(stream, capture, usize::from(length))?;
            println!("RFBViewerInformation ({length} bytes)");
        }
        other => {
            println!("client message 0x{other:02x}");
            // Unknown: assume ARD framing and read a length if it looks sane.
            let prefix = read_exact_capture(stream, capture, 3)?;
            let length = u16::from_be_bytes([prefix[1], prefix[2]]);
            if usize::from(length) < 65536 {
                read_exact_capture(stream, capture, usize::from(length))?;
            }
        }
    }
    Ok(None)
}

fn main() -> std::io::Result<()> {
    let port = env::args().nth(1).unwrap_or_else(|| "5999".to_owned());
    let host = env::args().nth(2).unwrap_or_else(|| "0.0.0.0".to_owned());
    let allowed_peer: IpAddr = env::args()
        .nth(3)
        .map_or(Ok("0.0.0.0".parse().unwrap()), |value| value.parse())
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    let authentication = env::args().nth(4).unwrap_or_else(|| "ard".to_owned());
    let out_dir = env::args().nth(5).unwrap_or_else(|| "/capture".to_owned());
    std::fs::create_dir_all(&out_dir)?;

    let listener = TcpListener::bind(format!("{host}:{port}"))?;
    println!("listening on vnc://{host}:{port}, capture dir {out_dir}");
    let udp_out_dir = out_dir.clone();
    let _udp_thread = std::thread::spawn(move || {
        for (label, port) in [("video1", 5901u16), ("video2", 5902), ("audio", 5903)] {
            let socket = UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], port))).ok();
            let Some(socket) = socket else { continue };
            let _ = socket.set_read_timeout(Some(std::time::Duration::from_millis(2500)));
            let file = format!("{udp_out_dir}/udp_{label}.bin");
            let writer = File::create(&file).ok();
            let Some(mut writer) = writer else { continue };
            let mut buffer = [0_u8; 2048];
            for _ in 0..8 {
                if let Ok((len, source)) = socket.recv_from(&mut buffer) {
                    let _ = writeln!(writer, "{} {}", source, hex(&buffer[..len]));
                }
            }
            let _ = writer.flush();
            println!("recorded UDP {label} datagrams -> {file}");
        }
    });
    loop {
        let (mut stream, peer) = loop {
            let (candidate, peer) = listener.accept()?;
            if allowed_peer.is_unspecified() || peer.ip() == allowed_peer {
                break (candidate, peer);
            }
            println!("rejected non-local peer {peer}");
        };
        println!("client connected from {peer}");
        if let Err(error) = handle_connection(&mut stream, &peer, &out_dir, &authentication) {
            println!("connection {peer} ended with error: {error}");
        }
    }
}

fn handle_connection(
    stream: &mut TcpStream,
    peer: &SocketAddr,
    out_dir: &str,
    authentication: &str,
) -> std::io::Result<()> {
    let capture = File::create(format!("{out_dir}/tcp_capture_{}.bin", peer.port()))?;
    let mut capture = BufWriter::new(capture);
    let session_log = File::create(format!("{out_dir}/session_{}.log", peer.port()))?;
    let mut session_log = BufWriter::new(session_log);

    if let Err(error) = stream.write_all(b"RFB 003.889\n") {
        println!("client closed before banner: {error}");
        return Ok(());
    }
    let mut banner = [0_u8; 12];
    if stream.read_exact(&mut banner).is_err() {
        println!("client closed before sending its banner");
        return Ok(());
    }
    log(
        &mut session_log,
        &format!("client banner: {}", String::from_utf8_lossy(&banner)),
    )?;

    let security_type = if authentication == "vnc" { 2 } else { 30 };
    stream.write_all(&[1, security_type])?;
    stream.flush()?;
    if security_type == 2 {
        vnc_authenticate(stream)?;
    } else {
        ard_send_challenge(stream)?;
        // Capture-proven exchange: the client sends 256 bytes of DH response
        // plus the 0xc1 ClientInit byte immediately, then waits for
        // SecurityResult before sending session options and SetEncodings.
        let mut dh_response = [0_u8; 257];
        stream.read_exact(&mut dh_response)?;
        if dh_response[256] != 0xc1 {
            println!("warning: ClientInit byte is {:#04x}", dh_response[256]);
        }
        stream.write_all(&0_u32.to_be_bytes())?;
        stream.flush()?;
    }
    println!("accepted authentication");

    let (width, height) = (1920_u16, 1080_u16);
    let name = b"ard-rs AVC oracle";
    let pixel_format = [
        32, 24, 0, 1, // bpp, depth, little-endian, true-colour
        0, 255, 0, 255, 0, 255, // channel maxima
        16, 8, 0, // channel shifts
        0, 0, 0, // padding
    ];
    // Plain ServerInit: the native client accepts this and proceeds normally
    // (verified by capture). The extended bitfield advertisement needs exact
    // replication and is left for a follow-up; this run checks whether the
    // client ever emits 0x1c on its own during a healthy session.
    let mut init = Vec::new();
    init.extend_from_slice(&width.to_be_bytes());
    init.extend_from_slice(&height.to_be_bytes());
    init.extend_from_slice(&pixel_format);
    init.extend_from_slice(&(name.len() as u32).to_be_bytes());
    init.extend_from_slice(name);
    stream.write_all(&init)?;
    stream.flush()?;
    println!("sent plain server initialization {width}x{height}");

    // Raw stream reader: keep the TCP session healthy (periodic raw
    // rectangle) while recording every client byte, flagging 0x1c.
    let mut buffer = [0_u8; 4096];
    let mut update_sent = false;
    let mut client_stream = Vec::new();
    loop {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => {
                println!("client disconnected");
                break;
            }
            Ok(len) => {
                let bytes = &buffer[..len];
                capture.write_all(bytes)?;
                capture.flush()?;
                log(
                    &mut session_log,
                    &format!("client sent {len} bytes: {}", hex(bytes)),
                )?;
                client_stream.extend_from_slice(bytes);
                if bytes.contains(&0x1c) {
                    println!(">>> client sent 0x1c (media stream configuration)!");
                    std::fs::write(format!("{out_dir}/avc_offer_{}.bin", peer.port()), bytes)
                        .expect("write offer");
                }
                // Only answer once the client has asked for a framebuffer
                // update (incremental request for the advertised size).
                let fbu_request = [0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x07, 0x80, 0x04, 0x38];
                if !update_sent && client_stream.windows(10).any(|w| w == fbu_request) {
                    send_raw_rectangle(stream)?;
                    update_sent = true;
                    println!("sent raw 8x8 rectangle");
                }
            }
        }
    }
    Ok(())
}

#[allow(dead_code)]
fn send_media_stream_rectangle(
    stream: &mut TcpStream,
    session_log: &mut BufWriter<File>,
) -> std::io::Result<()> {
    let message = MediaStreamMessage1 {
        encoding: ENCODING_AVC_MEDIA_STREAM,
        video1_port: 5901,
        video2_port: Some(5902),
        audio_port: Some(5903),
        video1_hdr: false,
        video2_hdr: false,
        stream_count: 1,
    };
    let struct_bytes = message.encode();
    let mut update = vec![0, 0, 0, 1]; // FramebufferUpdate, one rectangle
    update.extend_from_slice(&0u16.to_be_bytes()); // x
    update.extend_from_slice(&0u16.to_be_bytes()); // y
    update.extend_from_slice(&1920u16.to_be_bytes()); // w
    update.extend_from_slice(&1080u16.to_be_bytes()); // h
    update.extend_from_slice(&ENCODING_AVC_MEDIA_STREAM.to_be_bytes());
    update.extend_from_slice(&struct_bytes);
    log(
        session_log,
        &format!(
            "sending 1010 rectangle ({} bytes): {}",
            update.len(),
            hex(&update)
        ),
    )?;
    stream.write_all(&update)?;
    stream.flush()?;
    Ok(())
}

#[allow(dead_code)]
fn send_message1(
    stream: &mut TcpStream,
    session_log: &mut BufWriter<File>,
    out_dir: &str,
    peer: &SocketAddr,
) -> std::io::Result<()> {
    log(
        session_log,
        "client sent media stream configuration; replying with Message1",
    )?;
    let message = MediaStreamMessage1 {
        encoding: ENCODING_AVC_MEDIA_STREAM,
        video1_port: 5901,
        video2_port: Some(5902),
        audio_port: Some(5903),
        video1_hdr: false,
        video2_hdr: false,
        stream_count: 1,
    };
    let struct_bytes = message.encode();
    // Best-effort envelope: type + pad + u16 BE length + payload.
    // Payload = native builder bytes from offset 0xe (54 bytes).
    let payload = &struct_bytes[0xe..];
    let mut reply = vec![SERVER_MEDIA_STREAM_MESSAGE_TYPE, 0x00];
    reply.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    reply.extend_from_slice(payload);
    log(
        session_log,
        &format!(
            "sending Message1 reply ({} bytes): {}",
            reply.len(),
            hex(&reply)
        ),
    )?;
    std::fs::write(
        format!("{out_dir}/message1_reply_{}.bin", peer.port()),
        &reply,
    )
    .expect("write reply");
    stream.write_all(&reply)?;
    stream.flush()?;
    println!("sent Message1 reply ({} bytes)", reply.len());
    Ok(())
}
