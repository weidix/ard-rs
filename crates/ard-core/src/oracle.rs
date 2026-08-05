//! Pure-Rust test server for validating the modern encrypted transport
//! against Apple's Screen Sharing client.
//!
//! The server completes the type-30 exchange, advertises `0x12` command
//! support through the extended `ServerInit`, sends a real 1103
//! encryption-control rectangle, receives the client's activation message,
//! and then exchanges AES-CBC records containing MVS framebuffer updates.
//! Session keys and wrapped blocks are never printed or stored in reports.

use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};

use aes::Aes128;
use aes::cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};
use flate2::{Compress, Compression, FlushCompress};
use md5::{Digest, Md5};

use crate::{
    ArdEncryptionControl, ArdSessionMaterial, ArdSetEncryptionLevel, ArdViewerInformation,
    Encoding, PixelFormat, build_ard_server_init, parse_ard_set_encryption_level,
    parse_ard_viewer_information,
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

const MAX_PLAINTEXT_RECORD: usize = u16::MAX as usize;

/// One-shot ARD server that drives the full 1103 encrypted-record exchange.
#[derive(Debug, Clone)]
pub struct EncryptedTransportOracle {
    pub width: u16,
    pub height: u16,
    pub server_name: Vec<u8>,
    pub flags: u32,
    /// MSB-first command-support bitfield advertised in `ServerInit`.
    pub command_support: [u8; 16],
    pub session_value: [u8; 16],
    pub initial_chaining_value: [u8; 16],
    /// Optional server-to-client clipboard payload used by interoperability
    /// tests. The normal oracle leaves the clipboard stream disabled.
    pub server_clipboard_text: Option<Vec<u8>>,
    /// Reject peers other than this address. Defaults to loopback.
    pub allowed_peer: Option<IpAddr>,
    /// Fail when the client does not send the `0x12` proposal. Set to false
    /// to fall back to plain MVS frames for clients without encryption
    /// enabled.
    pub require_encryption: bool,
    /// Whether the client sends its one-byte security-type selection after
    /// the offer. Apple's Screen Sharing client does; the in-process Rust
    /// test client does not.
    pub expect_security_selection: bool,
    pub max_client_messages: usize,
    /// Test-only interoperability hook: close the session after this many
    /// server frames have been sent.
    pub close_after_frames: Option<usize>,
}

impl Default for EncryptedTransportOracle {
    fn default() -> Self {
        let mut command_support = [0_u8; 16];
        // Same low commands as the native default plus command 0x12
        // (RFBSetEncryptionLevel), stored MSB-first per byte.
        command_support[0] = 0xbe;
        command_support[2] = 0x20;
        Self {
            width: 64,
            height: 64,
            server_name: b"ard-rs encrypted oracle".to_vec(),
            flags: 8,
            command_support,
            session_value: [0x42; 16],
            initial_chaining_value: [0x24; 16],
            server_clipboard_text: None,
            allowed_peer: Some(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            require_encryption: true,
            expect_security_selection: true,
            max_client_messages: 64,
            close_after_frames: None,
        }
    }
}

/// Redacted summary of one oracle session. No key, wrapped block, password,
/// or derived value is included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OracleReport {
    pub peer: String,
    pub client_banner: [u8; 12],
    pub shared_session: bool,
    pub viewer_information: Option<ArdViewerInformation>,
    pub viewer_encodings: Vec<i32>,
    pub set_encryption_level: Option<ArdSetEncryptionLevel>,
    pub activation_received: bool,
    pub server_to_client_records: usize,
    pub client_to_server_records: usize,
    /// First byte (message type) of each decrypted client record.
    pub client_message_types: Vec<u8>,
    /// Incremental flag from every decrypted type-3 framebuffer request.
    pub client_framebuffer_update_incremental: Vec<bool>,
    pub frames_sent: usize,
}

impl EncryptedTransportOracle {
    pub fn run(&self, mut stream: TcpStream, peer: SocketAddr) -> io::Result<OracleReport> {
        let mut report = OracleReport {
            peer: peer.to_string(),
            client_banner: [0; 12],
            shared_session: false,
            viewer_information: None,
            viewer_encodings: Vec::new(),
            set_encryption_level: None,
            activation_received: false,
            server_to_client_records: 0,
            client_to_server_records: 0,
            client_message_types: Vec::new(),
            client_framebuffer_update_incremental: Vec::new(),
            frames_sent: 0,
        };

        stream.write_all(b"RFB 003.889\n")?;
        stream.read_exact(&mut report.client_banner)?;
        if &report.client_banner != b"RFB 003.889\n" {
            return Err(io::Error::other("client did not negotiate ARD 3.889"));
        }
        stream.write_all(&[1, 30])?;
        stream.flush()?;

        // Type-30 challenge. The server exponent is 1, so the shared integer
        // equals the client public key and the authentication value is
        // MD5(client_public_key).
        stream.write_all(&2_u16.to_be_bytes())?; // generator 2
        stream.write_all(&(DH_GROUP2_PRIME.len() as u16).to_be_bytes())?;
        stream.write_all(&DH_GROUP2_PRIME)?;
        let mut server_public_key = [0_u8; 128];
        server_public_key[127] = 2;
        stream.write_all(&server_public_key)?;
        stream.flush()?;

        let mut encrypted_credentials = [0_u8; 128];
        if self.expect_security_selection {
            let mut selection = [0_u8; 1];
            stream.read_exact(&mut selection)?;
            if selection[0] != 30 {
                encrypted_credentials[0] = selection[0];
                stream.read_exact(&mut encrypted_credentials[1..])?;
            } else {
                stream.read_exact(&mut encrypted_credentials)?;
            }
        } else {
            stream.read_exact(&mut encrypted_credentials)?;
        }
        let mut client_public_key = [0_u8; 128];
        stream.read_exact(&mut client_public_key)?;
        let authentication_value: [u8; 16] = Md5::digest(client_public_key).into();
        println!("received redacted type-30 response");
        // Do not log or retain the credential block or derived value.

        stream.write_all(&0_u32.to_be_bytes())?; // SecurityResult OK
        stream.flush()?;
        println!("sent SecurityResult");

        let mut shared = [0_u8; 1];
        stream.read_exact(&mut shared)?;
        report.shared_session = shared[0] != 0;
        println!("client init flags: {:#04x}", shared[0]);

        let init = build_ard_server_init(
            self.width,
            self.height,
            PixelFormat::XRGB8888,
            &self.server_name,
            self.flags,
            self.command_support,
        )
        .map_err(io::Error::other)?;
        stream.write_all(&init)?;
        stream.flush()?;
        println!("sent extended ServerInit ({} bytes)", init.len());

        self.receive_client_setup(&mut stream, &mut report)?;
        if report.set_encryption_level.is_none() {
            if self.require_encryption {
                return Err(io::Error::other(
                    "client did not send RFBSetEncryptionLevel; enable the \
                     Screen Sharing encryption requirement",
                ));
            }
            return self.send_plain_frames(&mut stream, report);
        }

        let control = self.build_control(authentication_value);
        send_control_rectangle(&mut stream, &control)?;

        let mut activation = [0_u8; 8];
        stream.read_exact(&mut activation)?;
        let (parsed, consumed) =
            parse_ard_set_encryption_level(&activation, 16).map_err(io::Error::other)?;
        if consumed != activation.len() || parsed.command != ArdSetEncryptionLevel::COMMAND_ACTIVATE
        {
            return Err(io::Error::other(
                "client did not send encryption activation",
            ));
        }
        report.activation_received = true;

        let material = ArdSessionMaterial::new(self.session_value, self.initial_chaining_value);
        let mut encoder = material
            .record_encoder(MAX_PLAINTEXT_RECORD)
            .map_err(io::Error::other)?;
        let mut decoder = material
            .record_decoder(MAX_PLAINTEXT_RECORD)
            .map_err(io::Error::other)?;

        // Exercise the first preferred family advertised by the client. MVS
        // validates the GPU-native path; full-colour zlib validates the
        // RDM-compatible lossless path with a persistent compression stream.
        let mut pending_frames: Vec<Vec<u8>> = if report.viewer_encodings.contains(&1011) {
            vec![
                mvs_white_rectangle(self.width, self.height),
                mvs_solid_ycbcr_rectangle(self.width, self.height, 200, 128, 128),
            ]
        } else {
            let mut compressor = Compress::new(Compression::default(), true);
            vec![
                zlib_solid_rectangle(&mut compressor, self.width, self.height, [255, 255, 255])?,
                zlib_solid_rectangle(&mut compressor, self.width, self.height, [32, 96, 192])?,
            ]
        };
        let first = pending_frames.remove(0);
        let record = encoder.encode_wire(&first).map_err(io::Error::other)?;
        stream.write_all(&record)?;
        stream.flush()?;
        report.server_to_client_records += 1;
        report.frames_sent += 1;

        if self
            .close_after_frames
            .is_some_and(|limit| report.frames_sent >= limit)
        {
            return Ok(report);
        }

        if let Some(text) = &self.server_clipboard_text {
            let mut clipboard = vec![3, 0, 0, 0];
            clipboard.extend_from_slice(
                &u32::try_from(text.len())
                    .map_err(|_| io::Error::other("clipboard test payload is too large"))?
                    .to_be_bytes(),
            );
            clipboard.extend_from_slice(text);
            let record = encoder.encode_wire(&clipboard).map_err(io::Error::other)?;
            stream.write_all(&record)?;
            stream.flush()?;
            report.server_to_client_records += 1;
        }

        // Read encrypted client records until EOF, responding to update
        // requests with the remaining frames.
        while report.client_message_types.len() < self.max_client_messages {
            let mut length = [0_u8; 2];
            match stream.read_exact(&mut length) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
                    ) =>
                {
                    break;
                }
                Err(error) => return Err(error),
            }
            let cipher_len = usize::from(u16::from_be_bytes(length));
            if cipher_len == 0 || !cipher_len.is_multiple_of(16) {
                return Err(io::Error::other("invalid client encrypted-record length"));
            }
            let mut ciphertext = vec![0_u8; cipher_len];
            stream.read_exact(&mut ciphertext)?;
            let payload = decoder.decode(&ciphertext).map_err(io::Error::other)?;
            report.client_to_server_records += 1;
            if let Some(&message_type) = payload.first() {
                report.client_message_types.push(message_type);
                if message_type == 3 {
                    if payload.len() != 10 {
                        return Err(io::Error::other(
                            "invalid framebuffer-update request message",
                        ));
                    }
                    report
                        .client_framebuffer_update_incremental
                        .push(payload[1] != 0);
                }
                if message_type == 9 && payload.len() != 16 {
                    return Err(io::Error::other(
                        "invalid automatic framebuffer-update message",
                    ));
                }
                if matches!(message_type, 3 | 9)
                    && let Some(frame) = pending_frames.first()
                {
                    let record = encoder.encode_wire(frame).map_err(io::Error::other)?;
                    stream.write_all(&record)?;
                    stream.flush()?;
                    report.server_to_client_records += 1;
                    report.frames_sent += 1;
                    pending_frames.remove(0);
                }
            }
        }
        Ok(report)
    }

    fn receive_client_setup(
        &self,
        stream: &mut TcpStream,
        report: &mut OracleReport,
    ) -> io::Result<()> {
        let mut messages = 0_usize;
        loop {
            messages += 1;
            if messages > self.max_client_messages {
                return Err(io::Error::other("too many client setup messages"));
            }
            let mut kind = [0_u8; 1];
            stream.read_exact(&mut kind)?;
            println!("client setup message: {:#04x}", kind[0]);
            match kind[0] {
                0x21 => {
                    let mut rest = vec![0_u8; 65];
                    stream.read_exact(&mut rest)?;
                    let mut message = vec![kind[0]];
                    message.extend_from_slice(&rest);
                    let (information, consumed) =
                        parse_ard_viewer_information(&message, 66).map_err(io::Error::other)?;
                    if consumed != message.len() {
                        return Err(io::Error::other("invalid viewer information length"));
                    }
                    report.viewer_information = Some(information);
                }
                0x12 => {
                    let mut rest = vec![0_u8; 11];
                    stream.read_exact(&mut rest)?;
                    let mut message = vec![kind[0]];
                    message.extend_from_slice(&rest);
                    let (parsed, consumed) =
                        parse_ard_set_encryption_level(&message, 16).map_err(io::Error::other)?;
                    if consumed != message.len() {
                        return Err(io::Error::other("invalid set-encryption-level length"));
                    }
                    if parsed.command == ArdSetEncryptionLevel::COMMAND_SET_METHODS {
                        report.set_encryption_level = Some(parsed);
                    }
                }
                0 => {
                    let mut payload = [0_u8; 19];
                    stream.read_exact(&mut payload)?;
                }
                2 => {
                    let mut header = [0_u8; 3];
                    stream.read_exact(&mut header)?;
                    let count = usize::from(u16::from_be_bytes([header[1], header[2]]));
                    if count > 256 {
                        return Err(io::Error::other("invalid encoding count"));
                    }
                    let mut encodings = vec![0_u8; count.saturating_mul(4)];
                    stream.read_exact(&mut encodings)?;
                    report.viewer_encodings = encodings
                        .chunks_exact(4)
                        .map(|encoding| {
                            i32::from_be_bytes(encoding.try_into().expect("encoding width checked"))
                        })
                        .collect();
                }
                3 => {
                    let mut request = [0_u8; 9];
                    stream.read_exact(&mut request)?;
                    return Ok(());
                }
                4 => {
                    let mut key_event = [0_u8; 7];
                    stream.read_exact(&mut key_event)?;
                }
                5 => {
                    let mut pointer_event = [0_u8; 5];
                    stream.read_exact(&mut pointer_event)?;
                }
                6 => {
                    let mut header = [0_u8; 7];
                    stream.read_exact(&mut header)?;
                    let length = usize::try_from(u32::from_be_bytes(
                        header[3..7].try_into().expect("cut-text length"),
                    ))
                    .map_err(|_| io::Error::other("invalid cut-text length"))?;
                    if length > MAX_PLAINTEXT_RECORD {
                        return Err(io::Error::other("cut-text length exceeds limit"));
                    }
                    let mut text = vec![0_u8; length];
                    stream.read_exact(&mut text)?;
                }
                10 => {
                    let mut options = [0_u8; 3];
                    stream.read_exact(&mut options)?;
                }
                other => {
                    return Err(io::Error::other(format!(
                        "unsupported client setup message {other}"
                    )));
                }
            }
        }
    }

    fn build_control(&self, authentication_value: [u8; 16]) -> ArdEncryptionControl {
        let cipher = Aes128::new(GenericArray::from_slice(&authentication_value));
        let mut wrapped = [self.session_value, self.initial_chaining_value];
        for block in &mut wrapped {
            cipher.encrypt_block(GenericArray::from_mut_slice(block));
        }
        ArdEncryptionControl::new(ArdEncryptionControl::ENABLE_COMMAND, wrapped)
            .expect("enable command is valid")
    }

    fn send_plain_frames(
        &self,
        stream: &mut TcpStream,
        mut report: OracleReport,
    ) -> io::Result<OracleReport> {
        let white = mvs_white_rectangle(self.width, self.height);
        send_mvs_rectangle(stream, 0, 0, self.width, self.height, &white)?;
        report.frames_sent += 1;
        let solid = mvs_solid_ycbcr_rectangle(self.width, self.height, 200, 128, 128);
        send_mvs_rectangle(stream, 0, 0, self.width, self.height, &solid)?;
        report.frames_sent += 1;
        let mut sink = [0_u8; 4096];
        while stream.read(&mut sink)? != 0 {}
        Ok(report)
    }
}

fn send_control_rectangle(
    stream: &mut TcpStream,
    control: &ArdEncryptionControl,
) -> io::Result<()> {
    let mut update = vec![0, 0, 0, 1];
    update.extend_from_slice(&[0; 8]);
    update.extend_from_slice(&(Encoding::ArdEncryption as i32).to_be_bytes());
    update.extend_from_slice(&control.command.to_be_bytes());
    for block in control.wrapped_session_blocks() {
        update.extend_from_slice(block);
    }
    stream.write_all(&update)?;
    stream.flush()
}

fn send_mvs_rectangle(
    stream: &mut TcpStream,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    payload: &[u8],
) -> io::Result<()> {
    let mut update = vec![0, 0, 0, 1];
    update.extend_from_slice(&x.to_be_bytes());
    update.extend_from_slice(&y.to_be_bytes());
    update.extend_from_slice(&width.to_be_bytes());
    update.extend_from_slice(&height.to_be_bytes());
    update.extend_from_slice(&(Encoding::ArdMvs as i32).to_be_bytes());
    update.extend_from_slice(payload);
    stream.write_all(&update)?;
    stream.flush()
}

fn push_bits(output: &mut Vec<bool>, value: u32, width: u8) {
    for shift in (0..width).rev() {
        output.push(value & (1 << shift) != 0);
    }
}

fn pack_bits(bits: &[bool]) -> Vec<u8> {
    let mut output = vec![0_u8; bits.len().div_ceil(8)];
    for (position, value) in bits.iter().copied().enumerate() {
        if value {
            output[position / 8] |= 0x80 >> (position % 8);
        }
    }
    output
}

fn frame_mvs_rectangle(payload: &[u8], width: u16, height: u16) -> Vec<u8> {
    let mut update = vec![0, 0, 0, 1];
    update.extend_from_slice(&0_u16.to_be_bytes());
    update.extend_from_slice(&0_u16.to_be_bytes());
    update.extend_from_slice(&width.to_be_bytes());
    update.extend_from_slice(&height.to_be_bytes());
    update.extend_from_slice(&(Encoding::ArdMvs as i32).to_be_bytes());
    update.extend_from_slice(payload);
    update
}

fn zlib_solid_rectangle(
    compressor: &mut Compress,
    width: u16,
    height: u16,
    rgb: [u8; 3],
) -> io::Result<Vec<u8>> {
    let pixels = usize::from(width)
        .checked_mul(usize::from(height))
        .ok_or_else(|| io::Error::other("oracle framebuffer size overflow"))?;
    let mut plain = Vec::with_capacity(pixels.saturating_mul(4));
    for _ in 0..pixels {
        // XRGB8888 is little-endian on the wire: B, G, R, unused.
        plain.extend_from_slice(&[rgb[2], rgb[1], rgb[0], 0]);
    }
    let before_out = compressor.total_out();
    let mut compressed = vec![0; plain.len().saturating_mul(2).saturating_add(128)];
    compressor
        .compress(&plain, &mut compressed, FlushCompress::Sync)
        .map_err(io::Error::other)?;
    let produced = usize::try_from(compressor.total_out() - before_out)
        .map_err(|_| io::Error::other("oracle compressed size overflow"))?;
    compressed.truncate(produced);

    let mut update = vec![0, 0, 0, 1];
    update.extend_from_slice(&0_u16.to_be_bytes());
    update.extend_from_slice(&0_u16.to_be_bytes());
    update.extend_from_slice(&width.to_be_bytes());
    update.extend_from_slice(&height.to_be_bytes());
    update.extend_from_slice(&(Encoding::Zlib as i32).to_be_bytes());
    let compressed_len = u32::try_from(compressed.len())
        .map_err(|_| io::Error::other("oracle compressed rectangle is too large"))?;
    update.extend_from_slice(&compressed_len.to_be_bytes());
    update.extend_from_slice(&compressed);
    Ok(update)
}

fn mvs_white_rectangle(width: u16, height: u16) -> Vec<u8> {
    let tiles = usize::from(width).div_ceil(8) * usize::from(height).div_ceil(8);
    let repeat = tiles - 1;
    let mut bits = Vec::new();
    push_bits(&mut bits, 0, 1); // initial state
    push_bits(&mut bits, 0, 3); // white tile update
    if repeat == 0 {
        push_bits(&mut bits, 0, 1);
    } else if repeat <= 15 {
        push_bits(&mut bits, 1, 1);
        push_bits(&mut bits, (repeat - 1) as u32, 4);
    } else {
        push_bits(&mut bits, 1, 1);
        push_bits(&mut bits, 15, 4);
        let mut value = repeat - 16;
        for group_index in 0..3 {
            let has_more = value >= 0x80 && group_index != 2;
            let group = (value & 0x7f) | (usize::from(has_more) * 0x80);
            push_bits(&mut bits, group as u32, 8);
            value >>= 7;
            if !has_more {
                break;
            }
        }
    }
    push_bits(&mut bits, 0x6d, 8); // primary marker

    let primary = pack_bits(&bits);
    let secondary_offset = 6 + primary.len();
    let mut update = vec![
        0,
        0,
        0,
        ((secondary_offset >> 16) & 0xff) as u8,
        ((secondary_offset >> 8) & 0xff) as u8,
        (secondary_offset & 0xff) as u8,
    ];
    update.extend_from_slice(&primary);
    update.push(0x6d); // secondary marker

    let mut framed = (update.len() as u32).to_be_bytes().to_vec();
    framed.extend_from_slice(&update);
    frame_mvs_rectangle(&framed, width, height)
}

fn mvs_solid_ycbcr_rectangle(width: u16, height: u16, y: u8, cb: u8, cr: u8) -> Vec<u8> {
    let tiles = usize::from(width).div_ceil(8) * usize::from(height).div_ceil(8);
    let mut primary_bits = Vec::new();
    push_bits(&mut primary_bits, 0, 1); // initial state
    push_bits(&mut primary_bits, 4, 3); // solid/two-colour update
    push_bits(&mut primary_bits, 0, 1); // no repeat
    let remaining = tiles - 1;
    if remaining > 0 {
        push_bits(&mut primary_bits, 0, 3); // white tile update
        let repeat = remaining - 1;
        if repeat <= 15 {
            push_bits(&mut primary_bits, 1, 1);
            push_bits(&mut primary_bits, repeat as u32, 4);
        } else {
            push_bits(&mut primary_bits, 1, 1); // extended repeat
            push_bits(&mut primary_bits, 15, 4);
            let mut value = repeat - 16;
            for group_index in 0..3 {
                let has_more = value >= 0x80 && group_index != 2;
                let group = (value & 0x7f) | (usize::from(has_more) * 0x80);
                push_bits(&mut primary_bits, group as u32, 8);
                value >>= 7;
                if !has_more {
                    break;
                }
            }
        }
    }
    push_bits(&mut primary_bits, 0x6d, 8); // primary marker
    let primary = pack_bits(&primary_bits);

    let mut secondary_bits = Vec::new();
    push_bits(&mut secondary_bits, 0, 1); // solid rather than two-colour
    push_bits(&mut secondary_bits, 0, 1); // transmit a new colour
    push_bits(&mut secondary_bits, u32::from(y), 8);
    push_bits(&mut secondary_bits, u32::from(cb >> 2), 6);
    push_bits(&mut secondary_bits, u32::from(cr >> 2), 6);
    push_bits(&mut secondary_bits, 0x6d, 8); // secondary marker
    let secondary = pack_bits(&secondary_bits);

    let secondary_offset = 6 + primary.len();
    let mut update = vec![
        0,
        0,
        0,
        ((secondary_offset >> 16) & 0xff) as u8,
        ((secondary_offset >> 8) & 0xff) as u8,
        (secondary_offset & 0xff) as u8,
    ];
    update.extend_from_slice(&primary);
    update.extend_from_slice(&secondary);

    let mut framed = (update.len() as u32).to_be_bytes().to_vec();
    framed.extend_from_slice(&update);
    frame_mvs_rectangle(&framed, width, height)
}

#[cfg(test)]
mod tests {
    use super::{mvs_solid_ycbcr_rectangle, mvs_white_rectangle};

    #[test]
    fn white_frame_is_a_complete_mvs_framebuffer_update() {
        let frame = mvs_white_rectangle(64, 64);
        assert_eq!(&frame[..4], &[0, 0, 0, 1]);
        assert_eq!(&frame[4..8], &[0, 0, 0, 0]); // x, y
        assert_eq!(&frame[8..12], &[0, 64, 0, 64]); // width, height
        assert_eq!(&frame[12..16], &[0, 0, 3, 0xf3]); // MVS 1011
        assert_eq!(&frame[16..20], &[0, 0, 0, 11]); // update length
        // Type 0, zero Rice parameters, secondary offset 10, then the
        // primary bitstream and both markers.
        assert_eq!(
            &frame[20..],
            &[0, 0, 0, 0, 0, 10, 0x0f, 0x97, 0xb6, 0x80, 0x6d]
        );
    }

    #[test]
    fn solid_frame_covers_one_solid_tile_and_white_rest() {
        let frame = mvs_solid_ycbcr_rectangle(64, 64, 200, 128, 128);
        assert_eq!(&frame[..4], &[0, 0, 0, 1]);
        assert_eq!(&frame[12..16], &[0, 0, 3, 0xf3]);
        assert_eq!(&frame[16..20], &[0, 0, 0, 14]);
        assert_eq!(
            &frame[20..],
            &[
                0, 0, 0, 0, 0, 10, 0x40, 0xf9, 0x73, 0x68, 0x32, 0x20, 0x81, 0xb4
            ]
        );
    }
}
