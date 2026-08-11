#![forbid(unsafe_code)]

//! Live AVC media-stream negotiation client (encoding 1010).
//!
//! Connects to a real macOS Screen Sharing server with Apple security type 30
//! (the same path `capture_real_desktop.rs` uses), establishes the ComCryption
//! encrypted RFB session, then performs the AVC negotiation:
//!
//! 1. sends the viewer's media-stream configuration (RFB client message `0x1c`)
//!    carrying the session UUID, audio/video1 offers and six 46-byte SRTP keys;
//! 2. waits for the server's `RFBMediaStreamMessage1` (encoding 1010) with the
//!    UDP ports and the negotiator answer (Message2);
//! 3. reports only the negotiated structure so the viewer pipeline can bind
//!    the sockets and start decoding without exposing media key material.
//!
//! Usage: `avc_client ADDRESS USERNAME [MAX_SECONDS]`
//!
//! The password is read from stdin and is never printed or stored.

use std::env;
use std::error::Error as StdError;
use std::fs::File;
use std::io::{self, BufRead, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use ard_rs::media_stream::{
    ENCODING_AVC_MEDIA_STREAM, MediaStreamConfiguration, MediaStreamFlags, MediaStreamKeyMaterial,
    MediaStreamMessage1, MediaStreamServerReply, build_media_stream_offer_with_ssrc,
    build_remote_endpoint_info,
};
use ard_rs::{
    ArdEncryptionControl, ArdMessageDispatcher, ArdViewerInformation, Decoder, Framebuffer,
    PixelFormat, ProtocolVersion, build_ard_encryption_activation, build_ard_set_encryption_level,
    build_ard_type30_client_exchange, build_set_encodings, build_set_pixel_format,
    parse_ard_auth_challenge, parse_security_types, parse_server_init, unwrap_ard_session_material,
};

const DEFAULT_MAX_SECONDS: u64 = 20;
const MAX_RECORD_BYTES: usize = u16::MAX as usize;
const MEDIA_STREAM_KEY_LEN: usize = 46;

fn main() -> Result<(), Box<dyn StdError>> {
    let address = env::args()
        .nth(1)
        .ok_or_else(|| io::Error::other("usage: avc_client ADDRESS USERNAME [MAX_SECONDS]"))?;
    let username = env::args()
        .nth(2)
        .ok_or_else(|| io::Error::other("usage: avc_client ADDRESS USERNAME [MAX_SECONDS]"))?;
    let max_seconds = env::args()
        .nth(3)
        .map(|value| value.parse::<u64>())
        .transpose()?
        .unwrap_or(DEFAULT_MAX_SECONDS);

    let mut password = read_password_from_stdin()?;
    let result = run(&address, username.as_bytes(), &password, max_seconds);
    password.fill(0);
    result
}

fn run(
    address: &str,
    username: &[u8],
    password: &[u8],
    max_seconds: u64,
) -> Result<(), Box<dyn StdError>> {
    let mut stream = TcpStream::connect(address)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(20)))?;
    stream.set_write_timeout(Some(Duration::from_secs(20)))?;

    // ---- RFB handshake + Apple security type 30 (same as capture_real_desktop)
    let banner = read_exact_vector(&mut stream, 12)?;
    if ProtocolVersion::parse(&banner)? != ProtocolVersion::ARD_3_889 {
        return Err(io::Error::other("server did not offer ARD 3.889").into());
    }
    stream.write_all(b"RFB 003.889\n")?;

    let security_count = usize::from(read_exact_vector(&mut stream, 1)?[0]);
    let mut security_offer = vec![security_count as u8];
    security_offer.extend_from_slice(&read_exact_vector(&mut stream, security_count)?);
    let (security_types, consumed) = parse_security_types(&security_offer, 36)?;
    if consumed != security_offer.len()
        || !security_types
            .iter()
            .any(|kind| matches!(kind, ard_rs::SecurityType::Apple(30)))
    {
        return Err(io::Error::other("server did not offer Apple security type 30").into());
    }
    stream.write_all(&[30])?;

    let mut challenge_wire = read_exact_vector(&mut stream, 4)?;
    let key_len = usize::from(u16::from_be_bytes([challenge_wire[2], challenge_wire[3]]));
    challenge_wire.extend_from_slice(&read_exact_vector(
        &mut stream,
        key_len
            .checked_mul(2)
            .ok_or_else(|| io::Error::other("authentication key length overflow"))?,
    )?);
    let (challenge, consumed) = parse_ard_auth_challenge(&challenge_wire, 512)?;
    if consumed != challenge_wire.len() {
        return Err(io::Error::other("trailing ARD authentication challenge bytes").into());
    }

    let mut private_random = vec![0_u8; key_len.saturating_mul(2)];
    fill_random(&mut private_random)?;
    let mut credential_noise = [0_u8; 128];
    fill_random(&mut credential_noise)?;
    let exchange = build_ard_type30_client_exchange(
        &challenge,
        username,
        password,
        &private_random,
        credential_noise,
        512,
    );
    private_random.fill(0);
    let exchange = exchange?;
    stream.write_all(&exchange.response().encrypted_credentials)?;
    stream.write_all(&exchange.response().client_public_key)?;
    stream.flush()?;

    let (_, mut authentication_value) = exchange.into_parts();
    let security_result = u32::from_be_bytes(
        read_exact_vector(&mut stream, 4)?
            .try_into()
            .expect("status"),
    );
    if security_result != 0 {
        authentication_value.fill(0);
        return Err(io::Error::other(format!(
            "Screen Sharing authentication failed with status {security_result}"
        ))
        .into());
    }

    stream.write_all(&[0xc1])?;
    let mut server_init_wire = read_exact_vector(&mut stream, 24)?;
    let payload_len = usize::try_from(u32::from_be_bytes(
        server_init_wire[20..24]
            .try_into()
            .expect("ServerInit length"),
    ))?;
    if payload_len > 1024 * 1024 {
        return Err(io::Error::other("ServerInit payload is too large").into());
    }
    server_init_wire.extend_from_slice(&read_exact_vector(&mut stream, payload_len)?);
    let (server_init, consumed) = parse_server_init(&server_init_wire, 1024 * 1024)?;
    if consumed != server_init_wire.len() {
        return Err(io::Error::other("trailing ServerInit bytes").into());
    }
    if !server_init
        .extension
        .as_ref()
        .is_some_and(|extension| extension.supports_command(0x12))
    {
        return Err(io::Error::other("server does not advertise encrypted transport").into());
    }

    let mut decoder = Decoder::new(PixelFormat::XRGB8888)?;
    let mut framebuffer = Framebuffer::new(server_init.width, server_init.height)?;

    stream.write_all(&[10, 0, 0, 1])?;
    stream.write_all(&viewer_information())?;
    stream.write_all(&build_set_pixel_format(PixelFormat::XRGB8888)?)?;
    stream.write_all(&build_set_encodings(&[1011, 6, 16, 0, -223])?)?;
    stream.write_all(&build_ard_set_encryption_level(1, &[1])?)?;
    stream.flush()?;

    let control = read_encryption_control(&mut stream, &mut decoder, &mut framebuffer)?;
    let material = unwrap_ard_session_material(&control, authentication_value);
    authentication_value.fill(0);
    stream.write_all(&build_ard_encryption_activation())?;

    let mut client_encoder = material.record_encoder(MAX_RECORD_BYTES)?;
    let mut server_decoder = material.record_decoder(MAX_RECORD_BYTES)?;

    // ---- Build and send the AVC media-stream configuration (0x1c)
    let mut session_id = [0_u8; 16];
    fill_random(&mut session_id)?;
    let key = |fill: &mut [u8; MEDIA_STREAM_KEY_LEN]| -> io::Result<()> { fill_random(fill) };
    let mut audio_v2s = [0_u8; MEDIA_STREAM_KEY_LEN];
    let mut audio_s2v = [0_u8; MEDIA_STREAM_KEY_LEN];
    let mut video1_v2s = [0_u8; MEDIA_STREAM_KEY_LEN];
    let mut video1_s2v = [0_u8; MEDIA_STREAM_KEY_LEN];
    key(&mut audio_v2s)?;
    key(&mut audio_s2v)?;
    key(&mut video1_v2s)?;
    key(&mut video1_s2v)?;
    let keys =
        MediaStreamKeyMaterial::new(&audio_v2s, &audio_s2v, &video1_v2s, &video1_s2v, None, None)?;

    let call_id = format_uuid(session_id);
    let mut video_call_id_bytes = [0_u8; 16];
    fill_random(&mut video_call_id_bytes)?;
    let video_call_id = format_uuid(video_call_id_bytes);
    let endpoint = build_remote_endpoint_info("Mac16,10", "25G72");
    let audio_ssrc = random_media_ssrc()?;
    let video1_ssrc = random_media_ssrc()?;
    let audio_offer = build_media_stream_offer_with_ssrc(&call_id, &endpoint, 8, 1, audio_ssrc)?;
    let video1_offer =
        build_media_stream_offer_with_ssrc(&video_call_id, &endpoint, 7, 2, video1_ssrc)?;
    let configuration = MediaStreamConfiguration {
        message_version: 0x0300,
        flags: MediaStreamFlags::new(
            MediaStreamFlags::VIDEO1_60FPS | MediaStreamFlags::SEND_CURSOR,
        ),
        session_id,
        audio_offer,
        video1_offer,
        video2_offer: None,
        keys,
    };
    let offer_bytes = configuration.encode()?;
    println!(
        "sending 0x1c media stream configuration ({} bytes)",
        offer_bytes.len()
    );
    stream.write_all(&client_encoder.encode_wire(&offer_bytes)?)?;
    stream.flush()?;
    drop(offer_bytes);
    drop(configuration);
    session_id.fill(0);
    audio_v2s.fill(0);
    audio_s2v.fill(0);
    video1_v2s.fill(0);
    video1_s2v.fill(0);
    video_call_id_bytes.fill(0);

    // ---- Wait for Message1 (1010) and the negotiator answer
    let deadline = Instant::now() + Duration::from_secs(max_seconds);
    let mut dispatcher = ArdMessageDispatcher::new(64 * 1024 * 1024, 1024 * 1024)?;
    let mut avc_window: Vec<u8> = Vec::new();
    let mut received_message1 = false;
    while Instant::now() < deadline {
        let ciphertext = match read_encrypted_record(&mut stream) {
            Ok(record) => record,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
            Err(error) if error.kind() == io::ErrorKind::TimedOut => continue,
            Err(error) => return Err(error.into()),
        };
        let plaintext = server_decoder.decode(&ciphertext)?;
        avc_window.extend_from_slice(&plaintext);
        if avc_window.len() > 1 << 20 {
            let excess = avc_window.len() - (1 << 20);
            avc_window.drain(..excess);
        }
        if let Some(reply) = scan_avc_reply(&avc_window) {
            match reply {
                MediaStreamServerReply::Message1(message1) => {
                    if !received_message1 {
                        received_message1 = true;
                        println!(
                            "Message1: video1={} video2={} audio={} hdr1={} hdr2={} streams={}",
                            message1.video1_port,
                            message1.video2_port.unwrap_or(0),
                            message1.audio_port.unwrap_or(0),
                            message1.video1_hdr,
                            message1.video2_hdr,
                            message1.stream_count
                        );
                    }
                }
                MediaStreamServerReply::Answer(answer) => {
                    println!(
                        "Answer: flags={:08x} fields={}/{}/{} body={} bytes",
                        answer.flags,
                        answer.field_a,
                        answer.field_b,
                        answer.field_c,
                        answer.answer_body.len()
                    );
                    println!("AVC negotiation completed successfully");
                    return Ok(());
                }
                MediaStreamServerReply::Error(error) => {
                    return Err(io::Error::other(format!(
                        "server rejected the media stream: type={} subcode={}",
                        error.error_type, error.error_sub_code
                    ))
                    .into());
                }
            }
            continue;
        }
        // The AVC reply (1010 rectangle / 0x23 message) may span several
        // encrypted records; do not feed it to the ordinary RFB dispatcher,
        // whose decoder rejects the 1010 rectangle encoding.
        if avc_window
            .windows(4)
            .any(|window| window == ENCODING_AVC_MEDIA_STREAM.to_be_bytes())
        {
            continue;
        }
        let messages = dispatcher
            .push(&plaintext, &mut decoder, &mut framebuffer)
            .map_err(|error| io::Error::other(format!("record parse: {error}")))?;
        for _message in messages {
            // Ordinary RFB messages; the AVC replies are handled above.
        }
    }
    Err(io::Error::other(format!(
        "timed out waiting for the AVC media stream reply (received_message1={received_message1}, window={} bytes)",
        avc_window.len()
    ))
    .into())
}

fn scan_avc_reply(plaintext: &[u8]) -> Option<MediaStreamServerReply> {
    // The server delivers Message1 as a FramebufferUpdate rectangle whose
    // encoding field is the big-endian value 1010; the 68-byte struct from
    // EncodeRFBMediaStreamMessage1 follows the rectangle header. The AVC
    // marker sits at struct offset 0x1a, so the struct body starts 0x1a
    // bytes before the marker.
    let marker = ENCODING_AVC_MEDIA_STREAM.to_be_bytes();
    for (index, window) in plaintext.windows(marker.len()).enumerate() {
        if window != marker {
            continue;
        }
        if index >= 0x1a {
            let start = index - 0x1a;
            if let Some(body) = plaintext.get(start..start + 0x44)
                && let Ok(message) = MediaStreamMessage1::parse(body)
            {
                return Some(MediaStreamServerReply::Message1(message));
            }
        }
        // Raw 0x23 server message: the body contains the marker at 0x1a.
        if index >= 0x1a {
            let start = index - 0x1a;
            if let Some(body) = plaintext.get(start..)
                && let Ok(reply) = MediaStreamServerReply::parse(0x23, body)
            {
                return Some(reply);
            }
        }
    }
    None
}

fn viewer_information() -> [u8; ArdViewerInformation::WIRE_LEN] {
    let mut info = [0_u8; ArdViewerInformation::WIRE_LEN];
    info[0] = 0x21;
    let size = (ArdViewerInformation::WIRE_LEN - 4) as u16;
    info[2..4].copy_from_slice(&size.to_be_bytes());
    info[4..6].copy_from_slice(&1_u16.to_be_bytes());
    info[6..10].copy_from_slice(&2_u32.to_be_bytes());
    info[10..14].copy_from_slice(&6_u32.to_be_bytes());
    info[14..18].copy_from_slice(&1_u32.to_be_bytes());
    info
}

fn format_uuid(bytes: [u8; 16]) -> String {
    format!(
        "{:08X}-{:04X}-{:04X}-{:04X}-{:012X}",
        u32::from_be_bytes(bytes[0..4].try_into().expect("uuid")),
        u16::from_be_bytes(bytes[4..6].try_into().expect("uuid")),
        u16::from_be_bytes(bytes[6..8].try_into().expect("uuid")),
        u16::from_be_bytes(bytes[8..10].try_into().expect("uuid")),
        u64::from_be_bytes([
            0, 0, bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        ])
    )
}

fn read_encryption_control(
    stream: &mut TcpStream,
    decoder: &mut Decoder,
    framebuffer: &mut Framebuffer,
) -> Result<ArdEncryptionControl, Box<dyn StdError>> {
    let mut buffer = Vec::new();
    let mut dispatcher = ArdMessageDispatcher::new(64 * 1024 * 1024, 1024 * 1024)?;
    for _ in 0..64 {
        buffer.clear();
        buffer.extend_from_slice(&read_exact_vector(stream, 52)?);
        for message in dispatcher.push(&buffer, decoder, framebuffer)? {
            if let ard_rs::ArdServerMessage::EncryptionControl(control) = message {
                return Ok(control);
            }
        }
    }
    Err(io::Error::other("no 1103 encryption control rectangle").into())
}

fn read_password_from_stdin() -> io::Result<Vec<u8>> {
    let mut password = Vec::new();
    io::stdin().lock().read_until(b'\n', &mut password)?;
    while password
        .last()
        .is_some_and(|byte| byte.is_ascii_whitespace())
    {
        password.pop();
    }
    Ok(password)
}

fn fill_random(bytes: &mut [u8]) -> io::Result<()> {
    getrandom(bytes).map_err(io::Error::other)
}

fn random_media_ssrc() -> io::Result<u32> {
    loop {
        let mut bytes = [0_u8; 4];
        fill_random(&mut bytes)?;
        let value = u32::from_be_bytes(bytes);
        if value != 0 {
            return Ok(value);
        }
    }
}

fn getrandom(bytes: &mut [u8]) -> Result<(), String> {
    let mut file = File::open("/dev/urandom").map_err(|error| error.to_string())?;
    file.read_exact(bytes).map_err(|error| error.to_string())
}

fn read_exact_vector(stream: &mut TcpStream, len: usize) -> io::Result<Vec<u8>> {
    let mut buffer = vec![0_u8; len];
    stream.read_exact(&mut buffer)?;
    Ok(buffer)
}

fn read_encrypted_record(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut length = [0_u8; 2];
    stream.read_exact(&mut length)?;
    let len = usize::from(u16::from_be_bytes(length));
    if len == 0 || len > MAX_RECORD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid encrypted-record length {len}"),
        ));
    }
    let mut record = vec![0_u8; len];
    stream.read_exact(&mut record)?;
    Ok(record)
}
