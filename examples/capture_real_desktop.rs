#![forbid(unsafe_code)]

//! Captures a complete real framebuffer from a macOS Screen Sharing
//! server. The password is read from stdin and is never printed or stored.

use std::env;
use std::error::Error as StdError;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ard_rs::{
    ArdEncryptionControl, ArdMessageDispatcher, ArdServerMessage, ArdViewerInformation, Decoder,
    Framebuffer, PixelFormat, ProtocolVersion, build_ard_encryption_activation,
    build_ard_set_encryption_level, build_ard_type30_client_exchange,
    build_framebuffer_update_request, build_set_encodings, build_set_pixel_format,
    parse_ard_auth_challenge, parse_framebuffer_update, parse_security_types, parse_server_init,
    unwrap_ard_session_material,
};

const MAX_RECORDS: usize = 512;
const MAX_RECORD_BYTES: usize = u16::MAX as usize;

fn main() -> Result<(), Box<dyn StdError>> {
    let address = env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:5900".to_owned());
    let username = env::args().nth(2).ok_or_else(|| {
        io::Error::other("usage: capture_real_desktop ADDRESS USERNAME OUTPUT_DIR")
    })?;
    let output_dir = env::args_os().nth(3).map(PathBuf::from).ok_or_else(|| {
        io::Error::other("usage: capture_real_desktop ADDRESS USERNAME OUTPUT_DIR")
    })?;
    fs::create_dir_all(&output_dir)?;

    let mut password = read_password_from_stdin()?;
    let result = capture(&address, username.as_bytes(), &password, &output_dir);
    password.fill(0);
    result
}

fn capture(
    address: &str,
    username: &[u8],
    password: &[u8],
    output_dir: &Path,
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
    let mut plaintext_stream = Vec::new();
    let raw_path = output_dir.join("real-ard-plaintext-stream.bin");
    let mut raw_file = BufWriter::new(File::create(&raw_path)?);
    let mut decoded_records = 0_usize;
    let mut framebuffer_updates = 0_usize;
    let mut recovered_full_frame = false;

    for _ in 0..MAX_RECORDS {
        let ciphertext = read_encrypted_record(&mut stream)?;
        let payload = server_decoder.decode(&ciphertext)?;
        decoded_records += 1;
        raw_file.write_all(&payload)?;
        raw_file.flush()?;
        plaintext_stream.extend_from_slice(&payload);

        let messages = dispatcher.push(&payload, &mut decoder, &mut framebuffer)?;
        for message in messages {
            if matches!(message, ArdServerMessage::FramebufferUpdate { .. }) {
                framebuffer_updates += 1;
            }
        }
        recovered_full_frame =
            crop_is_fully_decoded(&framebuffer, 0, 0, capture_width, capture_height);
        if recovered_full_frame {
            break;
        }
    }
    raw_file.flush()?;

    if !recovered_full_frame {
        return Err(io::Error::other(format!(
            "full framebuffer was not recovered after {decoded_records} encrypted records; plaintext saved at {}",
            raw_path.display()
        ))
        .into());
    }

    write_ppm_crop(
        &output_dir.join("real-frame-full.ppm"),
        &framebuffer,
        0,
        0,
        capture_width,
        capture_height,
    )?;
    fs::write(
        output_dir.join("metadata.txt"),
        format!(
            "source=live macOS screensharingd\nprotocol=RFB 003.889\nframebuffer_width={}\nframebuffer_height={}\ncapture_x=0\ncapture_y=0\ncapture_width={}\ncapture_height={}\nplaintext_bytes={}\nencrypted_records={}\nframebuffer_updates={}\n",
            server_init.width,
            server_init.height,
            capture_width,
            capture_height,
            plaintext_stream.len(),
            decoded_records,
            framebuffer_updates,
        ),
    )?;

    println!(
        "captured {} real ARD plaintext bytes from {} encrypted records; image={}x{}; output={}",
        plaintext_stream.len(),
        decoded_records,
        capture_width,
        capture_height,
        output_dir.display()
    );
    Ok(())
}

fn read_password_from_stdin() -> io::Result<Vec<u8>> {
    let mut password = Vec::new();
    io::stdin().read_to_end(&mut password)?;
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

fn crop_is_fully_decoded(
    framebuffer: &Framebuffer,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
) -> bool {
    (y..y + height).all(|row| {
        (x..x + width).all(|column| {
            let offset =
                (usize::from(row) * usize::from(framebuffer.width()) + usize::from(column)) * 4;
            framebuffer.rgba()[offset + 3] != 0
        })
    })
}

fn write_ppm_crop(
    path: &Path,
    framebuffer: &Framebuffer,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
) -> io::Result<()> {
    let mut output = BufWriter::new(File::create(path)?);
    write!(output, "P6\n{width} {height}\n255\n")?;
    for row in y..y + height {
        for column in x..x + width {
            let offset =
                (usize::from(row) * usize::from(framebuffer.width()) + usize::from(column)) * 4;
            output.write_all(&framebuffer.rgba()[offset..offset + 3])?;
        }
    }
    output.flush()
}
