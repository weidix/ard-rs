#![forbid(unsafe_code)]

//! Captures the decrypted ARD server-message stream from a live macOS
//! Screen Sharing session and replays it through the CPU decoder, saving the
//! plaintext, a per-update rectangle log, periodic frame snapshots, and any
//! decode errors for offline analysis.
//!
//! Usage: `capture_real_desktop ADDRESS USERNAME OUTPUT_DIR [MAX_SECONDS]`
//!
//! The password is read from stdin and is never printed or stored.

use std::env;
use std::error::Error as StdError;
use std::fs::{self, File};
use std::io::{self, BufRead, BufWriter, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ard_rs::{
    ArdEncryptionControl, ArdMessageDispatcher, ArdServerMessage, ArdViewerInformation, Decoder,
    Framebuffer, PixelFormat, ProtocolVersion, build_ard_encryption_activation,
    build_ard_set_encryption_level, build_ard_type30_client_exchange,
    build_framebuffer_update_request, build_set_encodings, build_set_pixel_format,
    parse_ard_auth_challenge, parse_framebuffer_update, parse_security_types, parse_server_init,
    unwrap_ard_session_material,
};

const DEFAULT_MAX_SECONDS: u64 = 30;
const MAX_RECORDS: usize = 20_000;
const MAX_RECORD_BYTES: usize = u16::MAX as usize;
const SNAPSHOT_EVERY_UPDATES: usize = 32;

fn main() -> Result<(), Box<dyn StdError>> {
    let address = env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:5900".to_owned());
    let username = env::args().nth(2).ok_or_else(|| {
        io::Error::other("usage: capture_real_desktop ADDRESS USERNAME OUTPUT_DIR [MAX_SECONDS]")
    })?;
    let output_dir = env::args_os().nth(3).map(PathBuf::from).ok_or_else(|| {
        io::Error::other("usage: capture_real_desktop ADDRESS USERNAME OUTPUT_DIR [MAX_SECONDS]")
    })?;
    let max_seconds = env::args()
        .nth(4)
        .map(|value| value.parse::<u64>())
        .transpose()?
        .unwrap_or(DEFAULT_MAX_SECONDS);
    fs::create_dir_all(&output_dir)?;

    let mut password = read_password_from_stdin()?;
    let result = capture(
        &address,
        username.as_bytes(),
        &password,
        &output_dir,
        max_seconds,
    );
    password.fill(0);
    result
}

fn capture(
    address: &str,
    username: &[u8],
    password: &[u8],
    output_dir: &Path,
    max_seconds: u64,
) -> Result<(), Box<dyn StdError>> {
    let mut stream = TcpStream::connect(address)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(20)))?;
    stream.set_write_timeout(Some(Duration::from_secs(20)))?;

    let banner = read_exact_vector(&mut stream, 12)?;
    let version = ProtocolVersion::parse(&banner)?;
    if version != ProtocolVersion::ARD_3_889 {
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
    let challenge_tail_len = key_len
        .checked_mul(2)
        .ok_or_else(|| io::Error::other("authentication key length overflow"))?;
    challenge_wire.extend_from_slice(&read_exact_vector(&mut stream, challenge_tail_len)?);
    let (challenge, consumed) = parse_ard_auth_challenge(&challenge_wire, 512)?;
    if consumed != challenge_wire.len() {
        return Err(io::Error::other("trailing ARD authentication challenge bytes").into());
    }

    let mut private_random = vec![0_u8; key_len.saturating_mul(2)];
    fill_random(&mut private_random)?;
    let mut credential_noise = [0_u8; 128];
    fill_random(&mut credential_noise)?;
    let exchange_result = build_ard_type30_client_exchange(
        &challenge,
        username,
        password,
        &private_random,
        credential_noise,
        512,
    );
    private_random.fill(0);
    let exchange = exchange_result?;
    stream.write_all(&exchange.response().encrypted_credentials)?;
    stream.write_all(&exchange.response().client_public_key)?;
    stream.flush()?;

    let (_, mut authentication_value) = exchange.into_parts();
    let security_result = u32::from_be_bytes(
        read_exact_vector(&mut stream, 4)?
            .try_into()
            .expect("security result length checked"),
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
            .expect("ServerInit length checked"),
    ))
    .map_err(|_| io::Error::other("ServerInit payload length overflow"))?;
    if payload_len > 1024 * 1024 {
        authentication_value.fill(0);
        return Err(io::Error::other("ServerInit payload is too large").into());
    }
    server_init_wire.extend_from_slice(&read_exact_vector(&mut stream, payload_len)?);
    let (server_init, consumed) = parse_server_init(&server_init_wire, 1024 * 1024)?;
    if consumed != server_init_wire.len() {
        authentication_value.fill(0);
        return Err(io::Error::other("trailing ServerInit bytes").into());
    }
    if !server_init
        .extension
        .as_ref()
        .is_some_and(|extension| extension.supports_command(0x12))
    {
        authentication_value.fill(0);
        return Err(io::Error::other("server does not advertise encrypted transport").into());
    }

    let capture_width = server_init.width;
    let capture_height = server_init.height;
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
    let request = build_framebuffer_update_request(false, 0, 0, capture_width, capture_height);
    stream.write_all(&client_encoder.encode_wire(&request)?)?;
    stream.flush()?;

    let mut dispatcher = ArdMessageDispatcher::new(64 * 1024 * 1024, 1024 * 1024)?;
    let plaintext_path = output_dir.join("real-ard-plaintext-stream.bin");
    let mut plaintext_file = BufWriter::new(File::create(&plaintext_path)?);
    let rectangle_log_path = output_dir.join("rectangles.log");
    let mut rectangle_log = BufWriter::new(File::create(&rectangle_log_path)?);
    let error_log_path = output_dir.join("decode-errors.log");
    let mut error_log = BufWriter::new(File::create(&error_log_path)?);

    let deadline = Instant::now() + Duration::from_secs(max_seconds);
    let mut decoded_records = 0_usize;
    let mut framebuffer_updates = 0_usize;
    let mut snapshot_sequence = 0_usize;
    let mut last_error: Option<String> = None;

    while decoded_records < MAX_RECORDS && Instant::now() < deadline {
        let ciphertext = match read_encrypted_record(&mut stream) {
            Ok(record) => record,
            Err(error) => {
                if error.kind() == io::ErrorKind::WouldBlock
                    || error.kind() == io::ErrorKind::TimedOut
                {
                    break;
                }
                return Err(error.into());
            }
        };
        let payload = server_decoder.decode(&ciphertext)?;
        decoded_records += 1;
        plaintext_file.write_all(&payload)?;
        plaintext_file.flush()?;

        match dispatcher.push(&payload, &mut decoder, &mut framebuffer) {
            Ok(messages) => {
                for message in &messages {
                    if let ArdServerMessage::FramebufferUpdate {
                        rectangle_count,
                        bytes,
                    } = message
                    {
                        framebuffer_updates += 1;
                        log_rectangles(
                            &mut rectangle_log,
                            framebuffer_updates,
                            *rectangle_count,
                            *bytes,
                            &payload,
                        )?;
                        if framebuffer_updates.is_multiple_of(SNAPSHOT_EVERY_UPDATES) {
                            write_ppm_crop(
                                &output_dir.join(format!("frame-{snapshot_sequence:05}.ppm")),
                                &framebuffer,
                                0,
                                0,
                                capture_width,
                                capture_height,
                            )?;
                            snapshot_sequence += 1;
                        }
                    }
                }
            }
            Err(error) => {
                let message = format!(
                    "decode error at record {decoded_records} after {framebuffer_updates} updates: {error}"
                );
                eprintln!("{message}");
                writeln!(error_log, "{message}")?;
                error_log.flush()?;
                last_error = Some(message);
            }
        }
    }
    plaintext_file.flush()?;
    rectangle_log.flush()?;

    write_ppm_crop(
        &output_dir.join("frame-final.ppm"),
        &framebuffer,
        0,
        0,
        capture_width,
        capture_height,
    )?;
    write_hex(
        &output_dir.join("real-ard-plaintext-stream.hex"),
        &fs::read(&plaintext_path)?,
    )?;
    fs::write(
        output_dir.join("metadata.txt"),
        format!(
            "source=live macOS screensharingd\nprotocol=RFB 003.889\nframebuffer_width={}\nframebuffer_height={}\ndecoded_records={}\nframebuffer_updates={}\nmax_seconds={}\nlast_error={}\n",
            server_init.width,
            server_init.height,
            decoded_records,
            framebuffer_updates,
            max_seconds,
            last_error.unwrap_or_else(|| "none".to_owned()),
        ),
    )?;

    println!(
        "captured {decoded_records} encrypted records ({framebuffer_updates} updates) from {address}; output={}",
        output_dir.display()
    );
    Ok(())
}

fn log_rectangles(
    output: &mut impl Write,
    sequence: usize,
    _rectangle_count: usize,
    message_bytes: usize,
    payload: &[u8],
) -> io::Result<()> {
    // Locate the message inside the record payload fragment. The dispatcher
    // already validated framing; this walker only logs the rectangles that
    // were present in this record's plaintext.
    let mut consumed = 0_usize;
    while consumed + 4 <= payload.len() {
        if payload[consumed] != 0 {
            consumed += 1;
            continue;
        }
        let count = usize::from(u16::from_be_bytes([
            payload[consumed + 2],
            payload[consumed + 3],
        ]));
        if count == 0 {
            consumed += 4;
            continue;
        }
        let mut cursor = consumed + 4;
        let mut found = false;
        for index in 0..count {
            if cursor + 12 > payload.len() {
                break;
            }
            let x = u16::from_be_bytes([payload[cursor], payload[cursor + 1]]);
            let y = u16::from_be_bytes([payload[cursor + 2], payload[cursor + 3]]);
            let width = u16::from_be_bytes([payload[cursor + 4], payload[cursor + 5]]);
            let height = u16::from_be_bytes([payload[cursor + 6], payload[cursor + 7]]);
            let encoding = i32::from_be_bytes(
                payload[cursor + 8..cursor + 12]
                    .try_into()
                    .expect("rectangle encoding length checked"),
            );
            writeln!(
                output,
                "update={sequence} rect={index} x={x} y={y} w={width} h={height} encoding={encoding} message_bytes={message_bytes}"
            )?;
            found = true;
            // Skip to the next rectangle: 12-byte header plus a payload whose
            // length depends on the encoding. Only the lengths needed for the
            // log walk are handled here; unknown encodings advance to the
            // message end so the log remains readable.
            let payload_len =
                rectangle_payload_len(encoding, width, height, &payload[cursor + 12..]);
            match payload_len {
                Some(len) => cursor += 12 + len,
                None => cursor = payload.len(),
            }
        }
        if found {
            break;
        }
        consumed += 4;
    }
    output.flush()
}

fn rectangle_payload_len(encoding: i32, width: u16, height: u16, tail: &[u8]) -> Option<usize> {
    match encoding {
        0 => {
            let bytes = usize::from(width) * usize::from(height) * 4;
            (bytes <= tail.len()).then_some(bytes)
        }
        1 => (tail.len() >= 4).then_some(4),
        6 | 16 | 1000 | 1001 | 1002 | 1011 => {
            if tail.len() < 4 {
                None
            } else {
                let len = usize::try_from(u32::from_be_bytes(
                    tail[..4].try_into().expect("length prefix checked"),
                ))
                .ok()?;
                (4 + len <= tail.len()).then_some(4 + len)
            }
        }
        1100 | 1103 | -223 => Some(0),
        _ => None,
    }
}

fn read_password_from_stdin() -> io::Result<Vec<u8>> {
    // Ask on the terminal so an interactive run does not silently block;
    // piped input still works because a single line is consumed either way.
    eprint!("Password: ");
    io::stderr().flush()?;
    let mut password = Vec::new();
    io::stdin().lock().read_until(b'\n', &mut password)?;
    while password
        .last()
        .is_some_and(|byte| matches!(byte, b'\n' | b'\r'))
    {
        password.pop();
    }
    if password.is_empty() {
        return Err(io::Error::other("empty password received on stdin"));
    }
    Ok(password)
}

fn fill_random(bytes: &mut [u8]) -> io::Result<()> {
    File::open("/dev/urandom")?.read_exact(bytes)
}

fn read_exact_vector(stream: &mut TcpStream, len: usize) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0_u8; len];
    stream.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn read_encrypted_record(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut length = [0_u8; 2];
    stream.read_exact(&mut length)?;
    let cipher_len = usize::from(u16::from_be_bytes(length));
    if cipher_len == 0 || !cipher_len.is_multiple_of(16) {
        return Err(io::Error::other("invalid encrypted-record length"));
    }
    read_exact_vector(stream, cipher_len)
}

fn read_encryption_control(
    stream: &mut TcpStream,
    decoder: &mut Decoder,
    framebuffer: &mut Framebuffer,
) -> Result<ArdEncryptionControl, Box<dyn StdError>> {
    loop {
        let message_type = read_exact_vector(stream, 1)?[0];
        match message_type {
            0 => {
                let mut update = vec![message_type];
                update.extend_from_slice(&read_exact_vector(stream, 3)?);
                let count = u16::from_be_bytes([update[2], update[3]]);
                if count != 1 {
                    return Err(
                        io::Error::other("expected a single encryption-control rectangle").into(),
                    );
                }
                let rectangle = read_exact_vector(stream, 12)?;
                let encoding = i32::from_be_bytes(
                    rectangle[8..12]
                        .try_into()
                        .expect("rectangle encoding length checked"),
                );
                if encoding != 1103 {
                    return Err(io::Error::other(format!(
                        "expected encoding 1103 control, received {encoding}"
                    ))
                    .into());
                }
                update.extend_from_slice(&rectangle);
                update
                    .extend_from_slice(&read_exact_vector(stream, ArdEncryptionControl::WIRE_LEN)?);
                let consumed = parse_framebuffer_update(&update, decoder, framebuffer)?;
                if consumed != update.len() {
                    return Err(io::Error::other("trailing encryption-control bytes").into());
                }
                return decoder
                    .take_ard_encryption_control()
                    .ok_or_else(|| io::Error::other("missing encryption-control payload").into());
            }
            2 => {}
            3 => {
                let header = read_exact_vector(stream, 7)?;
                let text_len = usize::try_from(u32::from_be_bytes(
                    header[3..7].try_into().expect("cut-text length checked"),
                ))
                .map_err(|_| io::Error::other("cut-text length overflow"))?;
                if text_len > 1024 * 1024 {
                    return Err(io::Error::other("cut-text message is too large").into());
                }
                let _ = read_exact_vector(stream, text_len)?;
            }
            other => {
                return Err(io::Error::other(format!(
                    "unexpected plaintext server message {other} before encryption"
                ))
                .into());
            }
        }
    }
}

fn viewer_information() -> [u8; ArdViewerInformation::WIRE_LEN] {
    let mut message = [0_u8; ArdViewerInformation::WIRE_LEN];
    message[0] = ArdViewerInformation::MESSAGE_TYPE;
    message[2..4].copy_from_slice(&(ArdViewerInformation::PAYLOAD_LEN as u16).to_be_bytes());
    message[4..6].copy_from_slice(&ArdViewerInformation::VERSION.to_be_bytes());
    for (index, component) in [2_u32, 6, 1, 0].into_iter().enumerate() {
        let offset = 6 + index * 4;
        message[offset..offset + 4].copy_from_slice(&component.to_be_bytes());
    }
    for (index, component) in [26_u32, 6, 0].into_iter().enumerate() {
        let offset = 22 + index * 4;
        message[offset..offset + 4].copy_from_slice(&component.to_be_bytes());
    }
    message
}

fn write_ppm_crop(
    path: &Path,
    framebuffer: &Framebuffer,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
) -> io::Result<()> {
    // The decoder retains XRGB8888 little-endian bytes: B, G, R, padding.
    let mut output = BufWriter::new(File::create(path)?);
    write!(output, "P6\n{width} {height}\n255\n")?;
    let stride = usize::from(framebuffer.width()) * 4;
    for row in y..y + height {
        for column in x..x + width {
            let offset = usize::from(row) * stride + usize::from(column) * 4;
            let pixel = &framebuffer.pixels()[offset..offset + 4];
            output.write_all(&[pixel[2], pixel[1], pixel[0]])?;
        }
    }
    output.flush()
}

fn write_hex(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut output = BufWriter::new(File::create(path)?);
    for line in bytes.chunks(16) {
        for (index, byte) in line.iter().enumerate() {
            if index != 0 {
                output.write_all(b" ")?;
            }
            write!(output, "{byte:02x}")?;
        }
        output.write_all(b"\n")?;
    }
    output.flush()
}
