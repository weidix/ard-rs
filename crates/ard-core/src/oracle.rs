//! Fixture-backed ARD server for protocol and end-to-end validation.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use aes::Aes128;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit, generic_array::GenericArray};
use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress};
use md5::{Digest, Md5};
use num_bigint::BigUint;

use crate::media_stream::negotiation::{
    MediaStreamCodec, MediaStreamOffer, build_media_stream_offer_with_ssrc_and_codec,
    build_remote_endpoint_info,
};
use crate::media_stream::srtp::SrtpContext;
use crate::media_stream::{
    ENCODING_AVC_MEDIA_STREAM, MediaStreamConfiguration, MediaStreamMessage1,
};
use crate::{
    ArdDisplaySelection, ArdEncryptionControl, ArdSessionMaterial, ArdSessionRecordEncoder,
    ArdSetEncryptionLevel, ArdViewerInformation, Encoding, PixelFormat, build_ard_server_init,
    parse_ard_set_encryption_level, parse_ard_viewer_information,
};

const WIDTH: u16 = 1920;
const HEIGHT: u16 = 1080;
const DISPLAY_FRAME_COUNT: usize = 300;
const MEDIA_ACCESS_UNIT_COUNT: usize = DISPLAY_FRAME_COUNT * 4;
const MAX_PLAINTEXT_RECORD: usize = u16::MAX as usize;
const MAX_RECORD_PAYLOAD: usize = 65_498;
const DEFAULT_FRAME_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 60);
const MEDIA_PAYLOAD_BYTES: usize = 1_150;
const MEDIA_BASE_SSRC: u32 = 0x6a11_0000;

const DH_PRIME_HEX: &str = concat!(
    "ffffffffffffffffc90fdaa22168c234c4c6628b80dc1cd129024e088a67cc74020bbea63b139b22514a08798e3404dd",
    "ef9519b3cd3a431b302b0a6df25f14374fe1356d6d51c245e485b576625e7ec6f44c42e9a637ed6b0bff5cb6f406b7ed",
    "ee386bfb5a899fa5ae9f24117c4b1fe649286651ece45b3dc2007cb8a163bf0598da48361c55d39a69163fa8fd24cf5f",
    "83655d23dca3ad961c62f356208552bb9ed529077096966d670c354e4abc9804f1746c08ca18217c32905e462e36ce3b",
    "e39e772c180e86039b2783a2ec07a28fb5c55df06f4c52c9de2bcbf6955817183995497cea956ae515d2261898fa0510",
    "15728e5a8aaac42dad33170d04507a33a85521abdf1cba64ecfb850458dbef0a8aea71575d060c7db3970f85a6e1e4c7",
    "abf5ae8cdb0933d71e8c94e04a25619dcee3d2261ad2ee6bf12ffa06d98a0864d87602733ec86a64521f2b18177b200c",
    "bbe117577a615d6c770988c0bad946e208e24fa074e5ab3143db5bfce0fd108e4b82d120a92108011a723c12a787e6d7",
    "88719a10bdba5b2699c327186af4e23c1a946834b6150bda2583e9ca2ad44ce8dbbbc2db04de8ef92e8efc141fbecaa6",
    "287c59474e6bc05d99b2964fa090c3a2233ba186515be7ed1f612970cee2d7afb81bdd762170481cd0069127d5b05aa9",
    "93b4ea988d8fddc186ffb7dc90a6c08f4df435c934063199ffffffffffffffff",
);
const DH_KEY_BYTES: usize = 512;
const DH_PRIVATE_KEY: [u8; 32] = [
    0x82, 0x96, 0x7d, 0x4f, 0xa3, 0x2b, 0x18, 0xc5, 0x71, 0xe9, 0x06, 0x3d, 0xbc, 0x54, 0x2a, 0x8f,
    0x39, 0xd1, 0x65, 0x7b, 0x24, 0xee, 0x90, 0x43, 0xaf, 0x12, 0xc8, 0x5d, 0x76, 0x31, 0x9b, 0xe7,
];

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum OracleMode {
    #[default]
    Auto,
    Halftone,
    Grayscale,
    Thousands,
    AdaptiveMvs,
    FullColor,
    H264,
    Hevc,
}

impl OracleMode {
    fn encoding(self) -> Option<i32> {
        match self {
            Self::Auto => None,
            Self::Halftone => Some(Encoding::ArdHalftone as i32),
            Self::Grayscale => Some(Encoding::ArdGrayscale as i32),
            Self::Thousands => Some(Encoding::ArdThousands as i32),
            Self::AdaptiveMvs => Some(Encoding::ArdMvs as i32),
            Self::FullColor => Some(Encoding::Zlib as i32),
            Self::H264 | Self::Hevc => Some(ENCODING_AVC_MEDIA_STREAM),
        }
    }

    fn codec(self) -> Option<MediaStreamCodec> {
        match self {
            Self::H264 => Some(MediaStreamCodec::H264),
            Self::Hevc => Some(MediaStreamCodec::Hevc),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OracleFixtures {
    pub h264: PathBuf,
    pub hevc: PathBuf,
    pub mvs: PathBuf,
    pub zlib: PathBuf,
}

impl OracleFixtures {
    pub fn from_dir(directory: impl AsRef<Path>) -> Self {
        let directory = directory.as_ref();
        Self {
            h264: directory.join("oracle-diagonal-frames-1920x1080-4x272.h264"),
            hevc: directory.join("oracle-diagonal-frames-1920x1080-4x272.h265"),
            mvs: directory.join("oracle-diagonal-frames-1920x1080.mvs"),
            zlib: directory.join("oracle-diagonal-frames-1920x1080.zlib"),
        }
    }

    pub fn repository() -> Self {
        Self::from_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/fixtures"))
    }

    pub fn validate(&self) -> io::Result<OracleFixtureSummary> {
        let summary = OracleFixtureSummary {
            mvs_frames: count_records(&self.mvs)?,
            zlib_frames: count_records(&self.zlib)?,
            h264_access_units: read_annex_b(&self.h264, MediaStreamCodec::H264)?.len(),
            hevc_access_units: read_annex_b(&self.hevc, MediaStreamCodec::Hevc)?.len(),
        };
        if summary
            != (OracleFixtureSummary {
                mvs_frames: DISPLAY_FRAME_COUNT,
                zlib_frames: DISPLAY_FRAME_COUNT,
                h264_access_units: MEDIA_ACCESS_UNIT_COUNT,
                hevc_access_units: MEDIA_ACCESS_UNIT_COUNT,
            })
        {
            return Err(io::Error::other(format!(
                "incomplete oracle fixtures: {summary:?}"
            )));
        }
        Ok(summary)
    }
}

impl Default for OracleFixtures {
    fn default() -> Self {
        Self::repository()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OracleFixtureSummary {
    pub mvs_frames: usize,
    pub zlib_frames: usize,
    pub h264_access_units: usize,
    pub hevc_access_units: usize,
}

#[derive(Debug, Clone)]
pub struct Oracle {
    pub width: u16,
    pub height: u16,
    pub server_name: Vec<u8>,
    pub flags: u32,
    pub command_support: [u8; 16],
    pub session_value: [u8; 16],
    pub initial_chaining_value: [u8; 16],
    pub server_clipboard_text: Option<Vec<u8>>,
    pub allowed_peer: Option<IpAddr>,
    pub require_encryption: bool,
    pub max_client_messages: usize,
    pub close_after_frames: Option<usize>,
    pub mode: OracleMode,
    pub fixtures: OracleFixtures,
    pub frame_interval: Duration,
}

impl Default for Oracle {
    fn default() -> Self {
        let mut command_support = [0_u8; 16];
        command_support[0] = 0xbe;
        command_support[2] = 0x20;
        Self {
            width: WIDTH,
            height: HEIGHT,
            server_name: b"ard-rs fixture oracle".to_vec(),
            flags: 0,
            command_support,
            session_value: [0x42; 16],
            initial_chaining_value: [0x24; 16],
            server_clipboard_text: None,
            allowed_peer: Some(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            require_encryption: true,
            max_client_messages: 64,
            close_after_frames: None,
            mode: OracleMode::Auto,
            fixtures: OracleFixtures::default(),
            frame_interval: DEFAULT_FRAME_INTERVAL,
        }
    }
}

pub type EncryptedTransportOracle = Oracle;

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
    pub client_message_types: Vec<u8>,
    pub client_display_selections: Vec<ArdDisplaySelection>,
    pub client_framebuffer_update_incremental: Vec<bool>,
    pub client_framebuffer_update_rectangles: Vec<(u16, u16, u16, u16)>,
    pub client_auto_frame_update_rectangles: Vec<(u16, u16, u16, u16)>,
    pub frames_sent: usize,
    pub selected_mode: Option<OracleMode>,
    pub media_configuration_received: bool,
}

impl OracleReport {
    fn new(peer: SocketAddr) -> Self {
        Self {
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
            client_display_selections: Vec::new(),
            client_framebuffer_update_incremental: Vec::new(),
            client_framebuffer_update_rectangles: Vec::new(),
            client_auto_frame_update_rectangles: Vec::new(),
            frames_sent: 0,
            selected_mode: None,
            media_configuration_received: false,
        }
    }
}

impl Oracle {
    pub fn run(&self, mut stream: TcpStream, peer: SocketAddr) -> io::Result<OracleReport> {
        if self
            .allowed_peer
            .is_some_and(|allowed| allowed != peer.ip())
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "oracle rejected peer",
            ));
        }
        if (self.width, self.height) != (WIDTH, HEIGHT) {
            return Err(io::Error::other(
                "fixture oracle framebuffer must be 1920x1080",
            ));
        }
        let mut report = OracleReport::new(peer);
        let authentication_value = self.authenticate(&mut stream, &mut report)?;
        self.receive_client_setup(&mut stream, &mut report)?;
        if report.set_encryption_level.is_none() {
            if self.require_encryption {
                return Err(io::Error::other(
                    "client did not request encrypted transport",
                ));
            }
            return self.run_plain(stream, report);
        }

        let control = build_control(
            self.session_value,
            self.initial_chaining_value,
            authentication_value,
        );
        send_control_rectangle(&mut stream, &control)?;
        eprintln!("oracle setup: encryption control sent");
        let mut activation = [0_u8; 8];
        stream.read_exact(&mut activation)?;
        let (parsed, consumed) =
            parse_ard_set_encryption_level(&activation, 16).map_err(io::Error::other)?;
        if consumed != activation.len() || parsed.command != ArdSetEncryptionLevel::COMMAND_ACTIVATE
        {
            return Err(io::Error::other("client did not activate encryption"));
        }
        report.activation_received = true;
        eprintln!("oracle setup: encryption activated");

        let material = ArdSessionMaterial::new(self.session_value, self.initial_chaining_value);
        let mut encoder = material
            .record_encoder(MAX_PLAINTEXT_RECORD)
            .map_err(io::Error::other)?;
        let mut decoder = material
            .record_decoder(MAX_PLAINTEXT_RECORD)
            .map_err(io::Error::other)?;
        let selected = self.select_mode(&report.viewer_encodings)?;
        report.selected_mode = Some(selected);
        let mut frames = selected
            .codec()
            .is_none()
            .then(|| RfbFrames::open(selected, &self.fixtures, self.width, self.height))
            .transpose()?;
        let mut media = selected
            .codec()
            .is_some()
            .then(|| {
                MediaOracle::bind(
                    stream.local_addr().map(|value| value.ip())?,
                    peer.ip(),
                    (self.mode != OracleMode::Auto)
                        .then(|| self.mode.codec())
                        .flatten(),
                )
            })
            .transpose()?;

        let mut plaintext = Vec::new();
        let mut automatic = false;
        let mut next_frame_at = Instant::now();
        if report
            .viewer_encodings
            .contains(&(Encoding::ArdDisplayInfo as i32))
        {
            let display_info = framebuffer_update(
                self.width,
                self.height,
                Encoding::ArdDisplayInfo as i32,
                &display_info_payload(self.width, self.height),
            );
            write_encrypted_message(
                &mut stream,
                &mut encoder,
                &display_info,
                &mut report.server_to_client_records,
            )?;
        }
        if report
            .viewer_encodings
            .contains(&(Encoding::ArdDisplayInfo2 as i32))
        {
            let display_info = framebuffer_update(
                self.width,
                self.height,
                Encoding::ArdDisplayInfo2 as i32,
                &display_info2_payload(self.width, self.height),
            );
            write_encrypted_message(
                &mut stream,
                &mut encoder,
                &display_info,
                &mut report.server_to_client_records,
            )?;
        }
        // Encrypted records arrive as `u16 length` followed by `length` bytes.
        //
        // The wire bytes are accumulated in `wire` and only consumed once a
        // whole record is present. Framing must never read straight into a
        // fixed-size buffer with `read_exact` and a read timeout: when the
        // deadline expires after some bytes have already been consumed,
        // `read_exact` reports `WouldBlock` and those bytes are lost, which
        // desynchronises every later record. That is what made the input tests
        // fail intermittently under load (a whole message, usually the
        // clipboard, silently disappeared from the report).
        let mut wire: Vec<u8> = Vec::new();
        let mut scratch = [0_u8; 8192];
        loop {
            // Framing runs *before* the automatic frame pump so a client message
            // that has already arrived is always handled before another frame is
            // written. Pumping frames first let a busy server starve the
            // client->server direction, which is another way the input tests
            // lost their final message under load.
            if wire.len() >= 2 {
                let cipher_len = usize::from(u16::from_be_bytes([wire[0], wire[1]]));
                if cipher_len == 0 || !cipher_len.is_multiple_of(16) {
                    return Err(io::Error::other("invalid encrypted-record length"));
                }
                if wire.len() >= 2 + cipher_len {
                    let record: Vec<u8> = wire.drain(..2 + cipher_len).skip(2).collect();
                    plaintext
                        .extend_from_slice(&decoder.decode(&record).map_err(io::Error::other)?);
                    report.client_to_server_records += 1;

                    while let Some(message_len) = encrypted_client_message_len(&plaintext)? {
                        let message: Vec<_> = plaintext.drain(..message_len).collect();
                        report.client_message_types.push(message[0]);
                        record_client_message(&message, &mut report);
                        match message[0] {
                            3 if frames.is_some() => {
                                if !self.send_next_frame(
                                    &mut stream,
                                    &mut encoder,
                                    frames.as_mut().unwrap(),
                                    &mut report,
                                )? {
                                    return Ok(report);
                                }
                            }
                            3 if media.is_some() => {
                                let bootstrap =
                                    media.as_ref().unwrap().bootstrap(self.width, self.height);
                                write_encrypted_message(
                                    &mut stream,
                                    &mut encoder,
                                    &bootstrap,
                                    &mut report.server_to_client_records,
                                )?;
                            }
                            9 if frames.is_some() => {
                                automatic = true;
                                next_frame_at = Instant::now() + self.frame_interval;
                            }
                            0x1c if media.is_some() => {
                                let configuration = MediaStreamConfiguration::parse(&message)
                                    .map_err(io::Error::other)?
                                    .0;
                                let (answer, codec) = media.as_mut().unwrap().answer(
                                    configuration,
                                    &self.fixtures,
                                    self.frame_interval,
                                    self.close_after_frames,
                                )?;
                                report.selected_mode = Some(match codec {
                                    MediaStreamCodec::H264 => OracleMode::H264,
                                    MediaStreamCodec::Hevc => OracleMode::Hevc,
                                });
                                write_encrypted_message(
                                    &mut stream,
                                    &mut encoder,
                                    &answer,
                                    &mut report.server_to_client_records,
                                )?;
                                report.media_configuration_received = true;
                            }
                            0x1d if media.is_some() => {
                                // A media-stream request may carry the display
                                // configuration; the codec is chosen by the
                                // offer handled in the 0x1c arm.
                            }
                            _ => {}
                        }
                        if report.client_message_types.len() >= self.max_client_messages {
                            return Ok(report);
                        }
                    }
                    // A single TCP read can carry several records; frame the
                    // next one without blocking on another read.
                    continue;
                }
            }

            if automatic && let Some(frames) = frames.as_mut() {
                let now = Instant::now();
                if now >= next_frame_at {
                    if !self.send_next_frame(&mut stream, &mut encoder, frames, &mut report)? {
                        break;
                    }
                    next_frame_at += self.frame_interval;
                    continue;
                }
                stream.set_read_timeout(Some(next_frame_at - now))?;
            } else {
                stream.set_read_timeout(None)?;
            }

            match stream.read(&mut scratch) {
                Ok(0) => break,
                Ok(read) => wire.extend_from_slice(&scratch[..read]),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    // `read` either moved whole bytes into `scratch` or nothing
                    // at all, so a timeout can never lose framed data.
                    continue;
                }
                Err(error) if connection_closed(&error) => break,
                Err(error) => return Err(error),
            }
        }
        Ok(report)
    }

    fn authenticate(
        &self,
        stream: &mut TcpStream,
        report: &mut OracleReport,
    ) -> io::Result<[u8; 16]> {
        stream.write_all(b"RFB 003.889\n")?;
        stream.read_exact(&mut report.client_banner)?;
        if &report.client_banner != b"RFB 003.889\n" {
            return Err(io::Error::other("client did not negotiate ARD 3.889"));
        }
        eprintln!("oracle handshake: ARD banner negotiated");
        stream.write_all(&[1, 30])?;
        stream.flush()?;
        // The native client always writes the one-byte security-type selection
        // (`_AuthenticateDHNamePassword` calls `WriteSocketData` with length 1
        // before reading the challenge) and `screensharingd`'s
        // `HandleAuthTypeMessage` always reads it. Reading it conditionally let
        // the client skip the byte whenever a single type was advertised, which
        // hid a real mutual-wait hang instead of failing the test.
        let mut selection = [0_u8; 1];
        stream.read_exact(&mut selection)?;
        eprintln!("oracle handshake: security type {} selected", selection[0]);
        if selection[0] != 30 {
            return Err(io::Error::other(format!(
                "client selected unsupported security type {}",
                selection[0]
            )));
        }

        let modulus = BigUint::parse_bytes(DH_PRIME_HEX.as_bytes(), 16)
            .expect("ARD DH modulus is a valid hexadecimal integer");
        let modulus_bytes = modulus.to_bytes_be();
        stream.write_all(&5_u16.to_be_bytes())?;
        stream.write_all(&(DH_KEY_BYTES as u16).to_be_bytes())?;
        stream.write_all(&modulus_bytes)?;
        let private_key = BigUint::from_bytes_be(&DH_PRIVATE_KEY);
        let server_public = BigUint::from(5_u8).modpow(&private_key, &modulus);
        let mut public_key = [0_u8; DH_KEY_BYTES];
        let server_public = server_public.to_bytes_be();
        let public_key_offset = public_key.len() - server_public.len();
        public_key[public_key_offset..].copy_from_slice(&server_public);
        stream.write_all(&public_key)?;
        stream.flush()?;
        eprintln!("oracle handshake: type-30 challenge sent");

        let mut response = [0_u8; 128 + DH_KEY_BYTES];
        let mut received = 0;
        while received < response.len() {
            match stream.read(&mut response[received..])? {
                0 => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!(
                            "received {received} of {} type-30 response bytes",
                            response.len()
                        ),
                    ));
                }
                count => {
                    received += count;
                    eprintln!(
                        "oracle handshake: received {received}/{} type-30 response bytes",
                        response.len()
                    );
                }
            }
        }
        let client_public_key = &response[128..];
        eprintln!("oracle handshake: type-30 response received");
        let shared_secret =
            BigUint::from_bytes_be(client_public_key).modpow(&private_key, &modulus);
        let mut shared_secret_bytes = [0_u8; DH_KEY_BYTES];
        let shared_secret = shared_secret.to_bytes_be();
        let shared_secret_offset = shared_secret_bytes.len() - shared_secret.len();
        shared_secret_bytes[shared_secret_offset..].copy_from_slice(&shared_secret);
        let authentication_value: [u8; 16] = Md5::digest(shared_secret_bytes).into();
        let cipher = Aes128::new(GenericArray::from_slice(&authentication_value));
        let mut credentials = response[..128].to_vec();
        for block in credentials.chunks_exact_mut(16) {
            cipher.decrypt_block(GenericArray::from_mut_slice(block));
        }
        let username_len = credentials[..64].iter().position(|&byte| byte == 0);
        let password_len = credentials[64..].iter().position(|&byte| byte == 0);
        eprintln!(
            "oracle handshake: credentials decrypted (username bytes: {username_len:?}, password bytes: {password_len:?})"
        );
        credentials.fill(0);
        stream.write_all(&0_u32.to_be_bytes())?;
        stream.flush()?;
        eprintln!("oracle handshake: SecurityResult sent");
        let mut shared = [0_u8; 1];
        stream.read_exact(&mut shared)?;
        eprintln!("oracle handshake: ClientInit received");
        report.shared_session = shared[0] != 0;
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
        eprintln!("oracle handshake: extended ServerInit sent");
        Ok(authentication_value)
    }

    fn receive_client_setup(
        &self,
        stream: &mut TcpStream,
        report: &mut OracleReport,
    ) -> io::Result<()> {
        for _ in 0..self.max_client_messages {
            let mut kind = [0_u8; 1];
            stream.read_exact(&mut kind)?;
            eprintln!("oracle setup: client message {:#04x}", kind[0]);
            match kind[0] {
                0x21 => {
                    let mut message = vec![kind[0]];
                    message.resize(66, 0);
                    stream.read_exact(&mut message[1..])?;
                    report.viewer_information = Some(
                        parse_ard_viewer_information(&message, 66)
                            .map_err(io::Error::other)?
                            .0,
                    );
                }
                0x12 => {
                    let mut message = vec![kind[0]];
                    message.resize(12, 0);
                    stream.read_exact(&mut message[1..])?;
                    let parsed = parse_ard_set_encryption_level(&message, 16)
                        .map_err(io::Error::other)?
                        .0;
                    if parsed.command == ArdSetEncryptionLevel::COMMAND_SET_METHODS {
                        report.set_encryption_level = Some(parsed);
                    }
                }
                0 => read_discard(stream, 19)?,
                2 => {
                    let mut header = [0_u8; 3];
                    stream.read_exact(&mut header)?;
                    let count = usize::from(u16::from_be_bytes([header[1], header[2]]));
                    let mut values = vec![0_u8; count * 4];
                    stream.read_exact(&mut values)?;
                    report.viewer_encodings = values
                        .chunks_exact(4)
                        .map(|value| i32::from_be_bytes(value.try_into().unwrap()))
                        .collect();
                    eprintln!("oracle setup: encodings {:?}", report.viewer_encodings);
                    if report.set_encryption_level.is_some() {
                        return Ok(());
                    }
                }
                3 => {
                    read_discard(stream, 9)?;
                    return Ok(());
                }
                4 => read_discard(stream, 7)?,
                5 => read_discard(stream, 5)?,
                6 => {
                    let mut header = [0_u8; 7];
                    stream.read_exact(&mut header)?;
                    read_discard(
                        stream,
                        u32::from_be_bytes(header[3..7].try_into().unwrap()) as usize,
                    )?;
                }
                10 => read_discard(stream, 3)?,
                other => {
                    return Err(io::Error::other(format!(
                        "unsupported setup message {other:#04x}"
                    )));
                }
            }
        }
        Err(io::Error::other("too many setup messages"))
    }

    fn select_mode(&self, encodings: &[i32]) -> io::Result<OracleMode> {
        if let Some(encoding) = self.mode.encoding() {
            return encodings
                .contains(&encoding)
                .then_some(self.mode)
                .ok_or_else(|| {
                    io::Error::other(format!("viewer did not offer encoding {encoding}"))
                });
        }
        [
            (ENCODING_AVC_MEDIA_STREAM, OracleMode::H264),
            (Encoding::ArdMvs as i32, OracleMode::AdaptiveMvs),
            (Encoding::ArdHalftone as i32, OracleMode::Halftone),
            (Encoding::ArdGrayscale as i32, OracleMode::Grayscale),
            (Encoding::ArdThousands as i32, OracleMode::Thousands),
            (Encoding::Zlib as i32, OracleMode::FullColor),
        ]
        .into_iter()
        .find_map(|(encoding, mode)| encodings.contains(&encoding).then_some(mode))
        .ok_or_else(|| io::Error::other("viewer offered no supported oracle encoding"))
    }

    fn send_next_frame(
        &self,
        stream: &mut TcpStream,
        encoder: &mut ArdSessionRecordEncoder,
        frames: &mut RfbFrames,
        report: &mut OracleReport,
    ) -> io::Result<bool> {
        let frame = match frames.next()? {
            Some(frame) => frame,
            None if self.close_after_frames.is_some() => return Ok(false),
            None => {
                frames.rewind()?;
                frames
                    .next()?
                    .ok_or_else(|| io::Error::other("oracle fixture has no frames"))?
            }
        };
        match write_encrypted_message(
            stream,
            encoder,
            &frame,
            &mut report.server_to_client_records,
        ) {
            Ok(()) => {}
            Err(error) if connection_closed(&error) => return Ok(false),
            Err(error) => return Err(error),
        }
        report.frames_sent += 1;
        if report.frames_sent == 1
            && let Some(text) = &self.server_clipboard_text
        {
            let mut clipboard = vec![3, 0, 0, 0];
            clipboard.extend_from_slice(&(text.len() as u32).to_be_bytes());
            clipboard.extend_from_slice(text);
            write_encrypted_message(
                stream,
                encoder,
                &clipboard,
                &mut report.server_to_client_records,
            )?;
        }
        Ok(!self
            .close_after_frames
            .is_some_and(|limit| report.frames_sent >= limit))
    }

    fn run_plain(
        &self,
        mut stream: TcpStream,
        mut report: OracleReport,
    ) -> io::Result<OracleReport> {
        let selected = self.select_mode(&report.viewer_encodings)?;
        report.selected_mode = Some(selected);
        let mut frames = RfbFrames::open(selected, &self.fixtures, self.width, self.height)?;
        while let Some(frame) = frames.next()? {
            stream.write_all(&frame)?;
            stream.flush()?;
            report.frames_sent += 1;
            if self
                .close_after_frames
                .is_some_and(|limit| report.frames_sent >= limit)
            {
                break;
            }
            thread::sleep(self.frame_interval);
        }
        Ok(report)
    }
}

fn connection_closed(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::UnexpectedEof
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
    )
}

fn read_discard(stream: &mut TcpStream, length: usize) -> io::Result<()> {
    let mut bytes = vec![0_u8; length];
    stream.read_exact(&mut bytes)
}

fn encrypted_client_message_len(bytes: &[u8]) -> io::Result<Option<usize>> {
    let Some(&message_type) = bytes.first() else {
        return Ok(None);
    };
    let length = match message_type {
        0 => 20,
        2 => {
            if bytes.len() < 4 {
                return Ok(None);
            }
            4 + usize::from(u16::from_be_bytes([bytes[2], bytes[3]])) * 4
        }
        3 => 10,
        4 => 8,
        5 => 6,
        9 => 16,
        0x0d => 8,
        0x10 => 8,
        0x17 => 58,
        6 => {
            if bytes.len() < 8 {
                return Ok(None);
            }
            8 + u32::from_be_bytes(bytes[4..8].try_into().unwrap()) as usize
        }
        0x1c | 0x1d => {
            if bytes.len() < 4 {
                return Ok(None);
            }
            4 + usize::from(u16::from_be_bytes([bytes[2], bytes[3]]))
        }
        other => {
            return Err(io::Error::other(format!(
                "unsupported encrypted client message type {other:#04x}"
            )));
        }
    };
    Ok((bytes.len() >= length).then_some(length))
}

fn record_client_message(message: &[u8], report: &mut OracleReport) {
    match message[0] {
        3 => {
            report
                .client_framebuffer_update_incremental
                .push(message[1] != 0);
            report.client_framebuffer_update_rectangles.push((
                u16::from_be_bytes([message[2], message[3]]),
                u16::from_be_bytes([message[4], message[5]]),
                u16::from_be_bytes([message[6], message[7]]),
                u16::from_be_bytes([message[8], message[9]]),
            ));
        }
        9 => report.client_auto_frame_update_rectangles.push((
            u16::from_be_bytes([message[8], message[9]]),
            u16::from_be_bytes([message[10], message[11]]),
            u16::from_be_bytes([message[12], message[13]]),
            u16::from_be_bytes([message[14], message[15]]),
        )),
        0x0d => report.client_display_selections.push(if message[1] != 0 {
            ArdDisplaySelection::Combined
        } else {
            ArdDisplaySelection::Display(u32::from_be_bytes(
                message[4..8]
                    .try_into()
                    .expect("display selection length checked"),
            ))
        }),
        _ => {}
    }
}

fn build_control(
    session_value: [u8; 16],
    initial_chaining_value: [u8; 16],
    authentication_value: [u8; 16],
) -> ArdEncryptionControl {
    let cipher = Aes128::new(GenericArray::from_slice(&authentication_value));
    let mut wrapped = [session_value, initial_chaining_value];
    for block in &mut wrapped {
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
    }
    ArdEncryptionControl::new(ArdEncryptionControl::ENABLE_COMMAND, wrapped).unwrap()
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

fn write_encrypted_message(
    stream: &mut TcpStream,
    encoder: &mut ArdSessionRecordEncoder,
    message: &[u8],
    records: &mut usize,
) -> io::Result<()> {
    for chunk in message.chunks(MAX_RECORD_PAYLOAD) {
        stream.write_all(&encoder.encode_wire(chunk).map_err(io::Error::other)?)?;
        *records += 1;
    }
    stream.flush()
}

fn framebuffer_update(width: u16, height: u16, encoding: i32, payload: &[u8]) -> Vec<u8> {
    let mut update = vec![0, 0, 0, 1, 0, 0, 0, 0];
    update.extend_from_slice(&width.to_be_bytes());
    update.extend_from_slice(&height.to_be_bytes());
    update.extend_from_slice(&encoding.to_be_bytes());
    update.extend_from_slice(payload);
    update
}

fn display_info_payload(width: u16, height: u16) -> Vec<u8> {
    let mut payload = Vec::with_capacity(38);
    payload.extend_from_slice(&0_u16.to_be_bytes());
    payload.extend_from_slice(&0_u16.to_be_bytes());
    payload.extend_from_slice(&0_u32.to_be_bytes());
    payload.extend_from_slice(&1_u16.to_be_bytes());
    payload.extend_from_slice(&1_u32.to_be_bytes());
    payload.extend_from_slice(&width.to_be_bytes());
    payload.extend_from_slice(&height.to_be_bytes());
    payload.extend_from_slice(&0_u32.to_be_bytes());
    payload.extend_from_slice(&[0; 16]);
    payload
}

fn display_info2_payload(width: u16, height: u16) -> Vec<u8> {
    let mut body = Vec::with_capacity(20 + 56);
    body.extend_from_slice(&5_u16.to_be_bytes());
    body.extend_from_slice(&width.to_be_bytes());
    body.extend_from_slice(&height.to_be_bytes());
    body.extend_from_slice(&width.to_be_bytes());
    body.extend_from_slice(&height.to_be_bytes());
    body.extend_from_slice(&u32::MAX.to_be_bytes());
    body.extend_from_slice(&0x0200_0000_u32.to_be_bytes());
    body.extend_from_slice(&1_u16.to_be_bytes());

    body.extend_from_slice(&1.0_f64.to_bits().to_be_bytes());
    body.extend_from_slice(&1.0_f64.to_bits().to_be_bytes());
    body.extend_from_slice(&1_u32.to_be_bytes());
    for rect in [[0, 0, height, width], [0, 0, height, width]] {
        for value in rect {
            body.extend_from_slice(&value.to_be_bytes());
        }
    }
    body.extend_from_slice(&1_u32.to_be_bytes());
    let format = PixelFormat::XRGB8888;
    body.extend_from_slice(&[
        format.bits_per_pixel,
        format.depth,
        u8::from(format.big_endian),
        u8::from(format.true_color),
    ]);
    body.extend_from_slice(&format.red_max.to_be_bytes());
    body.extend_from_slice(&format.green_max.to_be_bytes());
    body.extend_from_slice(&format.blue_max.to_be_bytes());
    body.extend_from_slice(&[
        format.red_shift,
        format.green_shift,
        format.blue_shift,
        0,
        0,
        0,
    ]);

    let mut payload = Vec::with_capacity(body.len() + 2);
    payload.extend_from_slice(&(body.len() as u16).to_be_bytes());
    payload.extend_from_slice(&body);
    payload
}

struct RecordReader {
    reader: BufReader<File>,
}

impl RecordReader {
    fn open(path: &Path) -> io::Result<Self> {
        Ok(Self {
            reader: BufReader::new(File::open(path)?),
        })
    }

    fn next(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut prefix = [0_u8; 4];
        match self.reader.read_exact(&mut prefix[..1]) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) => return Err(error),
        }
        self.reader.read_exact(&mut prefix[1..])?;
        let length = u32::from_be_bytes(prefix) as usize;
        let mut payload = Vec::with_capacity(4 + length);
        payload.extend_from_slice(&prefix);
        payload.resize(4 + length, 0);
        self.reader.read_exact(&mut payload[4..])?;
        Ok(Some(payload))
    }

    fn rewind(&mut self) -> io::Result<()> {
        self.reader.rewind()
    }
}

fn count_records(path: &Path) -> io::Result<usize> {
    let mut reader = RecordReader::open(path)?;
    let mut count = 0;
    while reader.next()?.is_some() {
        count += 1;
    }
    Ok(count)
}

enum RfbFrames {
    Direct {
        records: RecordReader,
        encoding: i32,
        width: u16,
        height: u16,
    },
    Converted {
        records: RecordReader,
        input: Decompress,
        output: Compress,
        mode: OracleMode,
        width: u16,
        height: u16,
    },
}

impl RfbFrames {
    fn open(
        mode: OracleMode,
        fixtures: &OracleFixtures,
        width: u16,
        height: u16,
    ) -> io::Result<Self> {
        match mode {
            OracleMode::AdaptiveMvs => Ok(Self::Direct {
                records: RecordReader::open(&fixtures.mvs)?,
                encoding: Encoding::ArdMvs as i32,
                width,
                height,
            }),
            OracleMode::FullColor => Ok(Self::Direct {
                records: RecordReader::open(&fixtures.zlib)?,
                encoding: Encoding::Zlib as i32,
                width,
                height,
            }),
            OracleMode::Halftone | OracleMode::Grayscale | OracleMode::Thousands => {
                Ok(Self::Converted {
                    records: RecordReader::open(&fixtures.zlib)?,
                    input: Decompress::new(true),
                    output: Compress::new(Compression::default(), true),
                    mode,
                    width,
                    height,
                })
            }
            _ => Err(io::Error::other("mode is not an RFB fixture mode")),
        }
    }

    fn next(&mut self) -> io::Result<Option<Vec<u8>>> {
        match self {
            Self::Direct {
                records,
                encoding,
                width,
                height,
            } => Ok(records
                .next()?
                .map(|payload| framebuffer_update(*width, *height, *encoding, &payload))),
            Self::Converted {
                records,
                input,
                output,
                mode,
                width,
                height,
            } => {
                let Some(record) = records.next()? else {
                    return Ok(None);
                };
                let pixels = usize::from(*width) * usize::from(*height);
                let mut xrgb = vec![0_u8; pixels * 4];
                let before = input.total_out();
                input
                    .decompress(&record[4..], &mut xrgb, FlushDecompress::Sync)
                    .map_err(io::Error::other)?;
                if input.total_out() - before != xrgb.len() as u64 {
                    return Err(io::Error::other("zlib fixture frame has the wrong size"));
                }
                let encoded = convert_pixels(*mode, &xrgb, *width, *height);
                let before = output.total_out();
                let mut compressed = vec![0_u8; encoded.len() * 2 + 128];
                output
                    .compress(&encoded, &mut compressed, FlushCompress::Sync)
                    .map_err(io::Error::other)?;
                compressed.truncate((output.total_out() - before) as usize);
                let mut payload = (compressed.len() as u32).to_be_bytes().to_vec();
                payload.extend_from_slice(&compressed);
                Ok(Some(framebuffer_update(
                    *width,
                    *height,
                    mode.encoding().unwrap(),
                    &payload,
                )))
            }
        }
    }

    fn rewind(&mut self) -> io::Result<()> {
        match self {
            Self::Direct { records, .. } => records.rewind(),
            Self::Converted { records, input, .. } => {
                records.rewind()?;
                input.reset(true);
                Ok(())
            }
        }
    }
}

fn convert_pixels(mode: OracleMode, xrgb: &[u8], width: u16, height: u16) -> Vec<u8> {
    fn luminance(pixel: &[u8]) -> u8 {
        let value = u32::from(pixel[2]) * 54 + u32::from(pixel[1]) * 183 + u32::from(pixel[0]) * 19;
        (value >> 8) as u8
    }
    match mode {
        OracleMode::Halftone => {
            let row_bytes = usize::from(width).div_ceil(8);
            let mut output = vec![0_u8; row_bytes * usize::from(height)];
            for (index, pixel) in xrgb.chunks_exact(4).enumerate() {
                let x = index % usize::from(width);
                let y = index / usize::from(width);
                if luminance(pixel) >= 128 {
                    output[y * row_bytes + x / 8] |= 0x80 >> (x % 8);
                }
            }
            output
        }
        OracleMode::Grayscale => {
            let row_bytes = usize::from(width).div_ceil(2);
            let mut output = vec![0_u8; row_bytes * usize::from(height)];
            for (index, pixel) in xrgb.chunks_exact(4).enumerate() {
                let x = index % usize::from(width);
                let y = index / usize::from(width);
                let value = luminance(pixel) >> 4;
                output[y * row_bytes + x / 2] |= if x.is_multiple_of(2) {
                    value << 4
                } else {
                    value
                };
            }
            output
        }
        OracleMode::Thousands => {
            let mut output = Vec::with_capacity(usize::from(width) * usize::from(height) * 2);
            for pixel in xrgb.chunks_exact(4) {
                let value = u16::from(pixel[2] >> 3) << 10
                    | u16::from(pixel[1] >> 3) << 5
                    | u16::from(pixel[0] >> 3);
                output.extend_from_slice(&value.to_be_bytes());
            }
            output
        }
        _ => unreachable!(),
    }
}

struct MediaOracle {
    socket: Option<UdpSocket>,
    peer_ip: IpAddr,
    port: u16,
    codec: Option<MediaStreamCodec>,
}

impl MediaOracle {
    fn bind(
        local_ip: IpAddr,
        peer_ip: IpAddr,
        codec: Option<MediaStreamCodec>,
    ) -> io::Result<Self> {
        let socket = UdpSocket::bind(SocketAddr::new(local_ip, 0))?;
        let port = socket.local_addr()?.port();
        Ok(Self {
            socket: Some(socket),
            peer_ip,
            port,
            codec,
        })
    }

    fn bootstrap(&self, width: u16, height: u16) -> Vec<u8> {
        let message = MediaStreamMessage1 {
            encoding: ENCODING_AVC_MEDIA_STREAM,
            video1_port: self.port,
            video2_port: None,
            audio_port: None,
            video1_hdr: false,
            video2_hdr: false,
            stream_count: 1,
        };
        framebuffer_update(width, height, ENCODING_AVC_MEDIA_STREAM, &message.encode())
    }

    fn answer(
        &mut self,
        configuration: MediaStreamConfiguration,
        fixtures: &OracleFixtures,
        interval: Duration,
        frame_limit: Option<usize>,
    ) -> io::Result<(Vec<u8>, MediaStreamCodec)> {
        let offer =
            MediaStreamOffer::parse(&configuration.video1_offer).map_err(io::Error::other)?;
        let requested = offer
            .codec
            .codec
            .ok_or_else(|| io::Error::other("media offer did not select H.264 or HEVC"))?;
        let codec = self.codec.unwrap_or(requested);
        if requested != codec {
            return Err(io::Error::other(
                "media offer codec does not match oracle mode",
            ));
        }

        let answer_body = build_media_stream_offer_with_ssrc_and_codec(
            "6A110000-0000-0000-0000-000000000001",
            &build_remote_endpoint_info("Mac16,12", "25G72"),
            7,
            2,
            MEDIA_BASE_SSRC,
            codec,
        )
        .map_err(io::Error::other)?;
        let body_len = 14 + answer_body.len();
        let mut compact = Vec::with_capacity(body_len + 2);
        compact.extend_from_slice(&(body_len as u16).to_be_bytes());
        compact.extend_from_slice(&0x0002_0002_u32.to_be_bytes());
        compact.extend_from_slice(&0_u32.to_be_bytes());
        compact.extend_from_slice(&[0; 6]);
        compact.extend_from_slice(&answer_body);

        let socket = self.socket.take().expect("one media offer per session");
        let destination = SocketAddr::new(self.peer_ip, self.port);
        let key = *configuration.keys.video1_server_to_viewer();
        let path = match codec {
            MediaStreamCodec::H264 => fixtures.h264.clone(),
            MediaStreamCodec::Hevc => fixtures.hevc.clone(),
        };
        thread::spawn(move || {
            let _ =
                stream_media_fixture(socket, destination, path, key, codec, interval, frame_limit);
        });
        Ok((
            framebuffer_update(WIDTH, HEIGHT, ENCODING_AVC_MEDIA_STREAM, &compact),
            codec,
        ))
    }
}

#[derive(Debug)]
struct FixtureAccessUnit {
    nal_units: Vec<Vec<u8>>,
}

fn read_annex_b(path: &Path, codec: MediaStreamCodec) -> io::Result<Vec<FixtureAccessUnit>> {
    let bytes = std::fs::read(path)?;
    let mut starts = Vec::new();
    let mut index = 0;
    while index + 3 < bytes.len() {
        let prefix = if bytes[index..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if bytes[index..].starts_with(&[0, 0, 1]) {
            3
        } else {
            index += 1;
            continue;
        };
        starts.push((index, prefix));
        index += prefix;
    }
    let mut nals = Vec::new();
    for (position, &(start, prefix)) in starts.iter().enumerate() {
        let end = starts
            .get(position + 1)
            .map_or(bytes.len(), |entry| entry.0);
        if start + prefix < end {
            nals.push(bytes[start + prefix..end].to_vec());
        }
    }

    let is_aud = |nal: &[u8]| match codec {
        MediaStreamCodec::H264 => nal[0] & 0x1f == 9,
        MediaStreamCodec::Hevc => (nal[0] >> 1) & 0x3f == 35,
    };
    let mut units = Vec::new();
    let mut current = Vec::new();
    let mut saw_aud = false;
    for nal in nals {
        if is_aud(&nal) {
            if saw_aud && !current.is_empty() {
                units.push(FixtureAccessUnit {
                    nal_units: std::mem::take(&mut current),
                });
            }
            saw_aud = true;
        } else {
            current.push(nal);
        }
    }
    if !current.is_empty() {
        units.push(FixtureAccessUnit { nal_units: current });
    }
    Ok(units)
}

fn stream_media_fixture(
    socket: UdpSocket,
    destination: SocketAddr,
    path: PathBuf,
    key: [u8; 46],
    codec: MediaStreamCodec,
    interval: Duration,
    frame_limit: Option<usize>,
) -> io::Result<()> {
    let units = read_annex_b(&path, codec)?;
    let limit = frame_limit
        .map(|frames| frames.saturating_mul(4))
        .unwrap_or(units.len())
        .min(units.len());
    let mut sequence = [0_u16; 4];
    let mut crypto = [
        SrtpContext::from_key_blob_with_derived_ssrc(&key, MEDIA_BASE_SSRC)
            .map_err(io::Error::other)?,
        SrtpContext::from_key_blob_with_derived_ssrc(&key, MEDIA_BASE_SSRC + 1)
            .map_err(io::Error::other)?,
        SrtpContext::from_key_blob_with_derived_ssrc(&key, MEDIA_BASE_SSRC + 2)
            .map_err(io::Error::other)?,
        SrtpContext::from_key_blob_with_derived_ssrc(&key, MEDIA_BASE_SSRC + 3)
            .map_err(io::Error::other)?,
    ];
    let started = Instant::now();
    for (index, unit) in units.into_iter().take(limit).enumerate() {
        let layer = index % 4;
        let timestamp = (index / 4) as u32 * 1_500;
        let payloads = packetize_access_unit(&unit, codec, index as u16);
        let count = payloads.len();
        for (packet_index, payload) in payloads.into_iter().enumerate() {
            let marker = packet_index + 1 == count;
            let payload_type = match codec {
                MediaStreamCodec::H264 => 123,
                MediaStreamCodec::Hevc => 100,
            };
            let mut packet = Vec::with_capacity(12 + payload.len() + 10);
            packet.push(0x80);
            packet.push(payload_type | if marker { 0x80 } else { 0 });
            packet.extend_from_slice(&sequence[layer].to_be_bytes());
            packet.extend_from_slice(&timestamp.to_be_bytes());
            packet.extend_from_slice(&(MEDIA_BASE_SSRC + layer as u32).to_be_bytes());
            packet.extend_from_slice(&payload);
            crypto[layer]
                .protect_rtp_packet(&mut packet, sequence[layer], 12)
                .map_err(io::Error::other)?;
            socket.send_to(&packet, destination)?;
            sequence[layer] = sequence[layer].wrapping_add(1);
        }
        if layer == 3 {
            let deadline = started + interval.mul_f64((index / 4 + 1) as f64);
            if let Some(delay) = deadline.checked_duration_since(Instant::now()) {
                thread::sleep(delay);
            }
        }
    }
    Ok(())
}

fn packetize_access_unit(
    unit: &FixtureAccessUnit,
    codec: MediaStreamCodec,
    don: u16,
) -> Vec<Vec<u8>> {
    let mut packets = Vec::new();
    for nal in &unit.nal_units {
        match codec {
            MediaStreamCodec::H264 if nal.len() + 5 <= MEDIA_PAYLOAD_BYTES => {
                let mut payload = vec![nal[0] & 0xe0 | 25];
                payload.extend_from_slice(&don.to_be_bytes());
                payload.extend_from_slice(&(nal.len() as u16).to_be_bytes());
                payload.extend_from_slice(nal);
                packets.push(payload);
            }
            MediaStreamCodec::H264 => {
                let header = nal[0];
                let chunks: Vec<_> = nal[1..].chunks(MEDIA_PAYLOAD_BYTES - 4).collect();
                let count = chunks.len();
                for (index, chunk) in chunks.into_iter().enumerate() {
                    let mut payload = vec![
                        header & 0xe0 | if index == 0 { 29 } else { 28 },
                        header & 0x1f,
                    ];
                    if index == 0 {
                        payload[1] |= 0x80;
                        payload.extend_from_slice(&don.to_be_bytes());
                    }
                    if index + 1 == count {
                        payload[1] |= 0x40;
                    }
                    payload.extend_from_slice(chunk);
                    packets.push(payload);
                }
            }
            MediaStreamCodec::Hevc if nal.len() + 6 <= MEDIA_PAYLOAD_BYTES => {
                let mut payload = vec![nal[0] & 0x81 | (48 << 1), nal[1]];
                payload.extend_from_slice(&don.to_be_bytes());
                payload.extend_from_slice(&(nal.len() as u16).to_be_bytes());
                payload.extend_from_slice(nal);
                packets.push(payload);
            }
            MediaStreamCodec::Hevc => {
                let nal_type = (nal[0] >> 1) & 0x3f;
                let chunks: Vec<_> = nal[2..].chunks(MEDIA_PAYLOAD_BYTES - 5).collect();
                let count = chunks.len();
                for (index, chunk) in chunks.into_iter().enumerate() {
                    let mut payload = vec![nal[0] & 0x81 | (49 << 1), nal[1], nal_type];
                    if index == 0 {
                        payload[2] |= 0x80;
                    }
                    if index + 1 == count {
                        payload[2] |= 0x40;
                    }
                    payload.extend_from_slice(&don.to_be_bytes());
                    payload.extend_from_slice(chunk);
                    packets.push(payload);
                }
            }
        }
    }
    packets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_native_post_activation_client_messages() {
        assert_eq!(encrypted_client_message_len(&[0; 20]).unwrap(), Some(20));
        assert_eq!(
            encrypted_client_message_len(&[2, 0, 0, 1, 0, 0, 0, 6]).unwrap(),
            Some(8)
        );
        assert_eq!(
            encrypted_client_message_len(&[0x10, 0, 0, 0, 0, 0, 0, 0]).unwrap(),
            Some(8)
        );
    }

    #[test]
    fn display_info_matches_the_native_single_display_layout() {
        let payload = display_info_payload(1920, 1080);
        assert_eq!(payload.len(), 10 + 28);
        assert_eq!(&payload[..10], &[0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(u32::from_be_bytes(payload[10..14].try_into().unwrap()), 1);
        assert_eq!(
            u16::from_be_bytes(payload[14..16].try_into().unwrap()),
            1920
        );
        assert_eq!(
            u16::from_be_bytes(payload[16..18].try_into().unwrap()),
            1080
        );
        assert_eq!(&payload[18..], &[0; 20]);
    }

    #[test]
    fn display_info2_matches_the_single_display_layout() {
        let payload = display_info2_payload(1920, 1080);
        let (layout, consumed) = crate::parse_ard_display_info2(&payload).unwrap();
        assert_eq!(consumed, payload.len());
        assert_eq!(
            (layout.framebuffer_width, layout.framebuffer_height),
            (1920, 1080)
        );
        assert_eq!(layout.current_display, None);
        assert_eq!(layout.displays.len(), 1);
        assert_eq!(layout.displays[0].id, 1);
        assert_eq!(
            (
                layout.displays[0].framebuffer_bounds.width,
                layout.displays[0].framebuffer_bounds.height
            ),
            (1920, 1080)
        );
    }

    #[test]
    fn repository_fixtures_cover_every_oracle_path() {
        assert_eq!(
            OracleFixtures::repository().validate().unwrap(),
            OracleFixtureSummary {
                mvs_frames: 300,
                zlib_frames: 300,
                h264_access_units: 1200,
                hevc_access_units: 1200,
            }
        );
    }

    #[test]
    fn every_rfb_mode_reads_the_first_fixture_frame() {
        let fixtures = OracleFixtures::repository();
        for mode in [
            OracleMode::Halftone,
            OracleMode::Grayscale,
            OracleMode::Thousands,
            OracleMode::AdaptiveMvs,
            OracleMode::FullColor,
        ] {
            let frame = RfbFrames::open(mode, &fixtures, WIDTH, HEIGHT)
                .unwrap()
                .next()
                .unwrap()
                .unwrap();
            assert_eq!(&frame[..4], &[0, 0, 0, 1]);
            assert_eq!(
                i32::from_be_bytes(frame[12..16].try_into().unwrap()),
                mode.encoding().unwrap()
            );
        }
    }

    #[test]
    fn media_packetizers_cover_both_samples() {
        use crate::{H264Depacketizer, HevcDepacketizer, RtpPacket};

        let fixtures = OracleFixtures::repository();
        for (codec, path) in [
            (MediaStreamCodec::H264, fixtures.h264),
            (MediaStreamCodec::Hevc, fixtures.hevc),
        ] {
            let units = read_annex_b(&path, codec).unwrap();
            let mut h264 = H264Depacketizer::new();
            let mut hevc = HevcDepacketizer::new_with_donl();
            let mut sequence = 0_u16;
            for (don, unit) in units.iter().enumerate() {
                let packets = packetize_access_unit(unit, codec, don as u16);
                assert!(!packets.is_empty());
                assert!(
                    packets
                        .iter()
                        .all(|packet| packet.len() <= MEDIA_PAYLOAD_BYTES)
                );
                let packet_count = packets.len();
                let mut decoded = None;
                for (index, payload) in packets.into_iter().enumerate() {
                    let mut wire = vec![
                        0x80,
                        match codec {
                            MediaStreamCodec::H264 => 123,
                            MediaStreamCodec::Hevc => 100,
                        } | if index + 1 == packet_count { 0x80 } else { 0 },
                    ];
                    wire.extend_from_slice(&sequence.to_be_bytes());
                    wire.extend_from_slice(&((don / 4) as u32 * 1_500).to_be_bytes());
                    wire.extend_from_slice(&MEDIA_BASE_SSRC.to_be_bytes());
                    wire.extend_from_slice(&payload);
                    let packet = RtpPacket::parse(&wire).unwrap();
                    decoded = match codec {
                        MediaStreamCodec::H264 => h264.push(&packet).unwrap(),
                        MediaStreamCodec::Hevc => hevc.push(&packet).unwrap(),
                    };
                    sequence = sequence.wrapping_add(1);
                }
                let decoded = decoded.expect("marked packet completes access unit");
                assert_eq!(decoded.decode_order_number, Some(don as u16));
                assert_eq!(decoded.nal_units, unit.nal_units);
            }
        }
    }

    #[test]
    fn every_committed_rfb_frame_decodes() {
        use crate::{Decoder, Framebuffer, FramebufferFormat, Rectangle};

        let fixtures = OracleFixtures::repository();
        let mut records = RecordReader::open(&fixtures.mvs).unwrap();
        let mut decoder = Decoder::new_gpu_mvs(PixelFormat::XRGB8888).unwrap();
        let mut framebuffer = Framebuffer::new_metadata_with_format(
            WIDTH,
            HEIGHT,
            FramebufferFormat::Native(PixelFormat::XRGB8888),
        )
        .unwrap();
        let rect = Rectangle {
            x: 0,
            y: 0,
            width: WIDTH,
            height: HEIGHT,
            encoding: Encoding::ArdMvs as i32,
        };
        let mut mvs_count = 0;
        while let Some(payload) = records.next().unwrap() {
            assert_eq!(
                decoder
                    .decode_complete_rectangle(rect, &payload, &mut framebuffer)
                    .unwrap(),
                payload.len()
            );
            assert_eq!(decoder.take_gpu_mvs_frames().len(), 1);
            mvs_count += 1;
        }
        assert_eq!(mvs_count, DISPLAY_FRAME_COUNT);

        let mut records = RecordReader::open(&fixtures.zlib).unwrap();
        let mut decoder = Decompress::new(true);
        let mut pixels = vec![0_u8; usize::from(WIDTH) * usize::from(HEIGHT) * 4];
        let mut zlib_count = 0;
        while let Some(payload) = records.next().unwrap() {
            let before_in = decoder.total_in();
            let before_out = decoder.total_out();
            decoder
                .decompress(&payload[4..], &mut pixels, FlushDecompress::Sync)
                .unwrap();
            assert_eq!(decoder.total_in() - before_in, (payload.len() - 4) as u64);
            assert_eq!(decoder.total_out() - before_out, pixels.len() as u64);
            zlib_count += 1;
        }
        assert_eq!(zlib_count, DISPLAY_FRAME_COUNT);
    }
}
