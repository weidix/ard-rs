use crate::wire::Cursor;
use crate::{Decoder, Error, Framebuffer, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtocolVersion {
    pub const ARD_3_889: Self = Self {
        major: 3,
        minor: 889,
    };

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 12 {
            return Err(Error::NeedMore {
                needed: 12,
                available: bytes.len(),
            });
        }
        if &bytes[..4] != b"RFB " || bytes[7] != b'.' || bytes[11] != b'\n' {
            return Err(Error::Invalid("invalid RFB/ARD version banner"));
        }
        let major = parse_three_digits(&bytes[4..7])?;
        let minor = parse_three_digits(&bytes[8..11])?;
        Ok(Self { major, minor })
    }

    pub fn banner(self) -> Result<[u8; 12]> {
        if self.major > 999 || self.minor > 999 {
            return Err(Error::Invalid("version field does not fit the banner"));
        }
        let text = format!("RFB {:03}.{:03}\n", self.major, self.minor);
        Ok(text.as_bytes().try_into().expect("fixed-width format"))
    }
}

fn parse_three_digits(bytes: &[u8]) -> Result<u16> {
    if bytes.len() != 3 || !bytes.iter().all(u8::is_ascii_digit) {
        return Err(Error::Invalid("invalid version number"));
    }
    Ok(u16::from(bytes[0] - b'0') * 100
        + u16::from(bytes[1] - b'0') * 10
        + u16::from(bytes[2] - b'0'))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityType {
    None,
    VncAuthentication,
    Apple(u8),
    Other(u8),
}

impl From<u8> for SecurityType {
    fn from(value: u8) -> Self {
        match value {
            1 => Self::None,
            2 => Self::VncAuthentication,
            30..=36 => Self::Apple(value),
            value => Self::Other(value),
        }
    }
}

pub fn parse_security_types(bytes: &[u8], max_types: usize) -> Result<(Vec<SecurityType>, usize)> {
    let mut cursor = Cursor::new(bytes);
    let count = usize::from(cursor.u8()?);
    if count == 0 {
        return Err(Error::Invalid("server rejected the connection"));
    }
    if count > max_types {
        return Err(Error::LimitExceeded("security type count"));
    }
    let values = cursor
        .take(count)?
        .iter()
        .copied()
        .map(Into::into)
        .collect();
    Ok((values, cursor.position()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArdAuthChallenge {
    pub generator: u16,
    pub prime: Vec<u8>,
    pub server_public_key: Vec<u8>,
}

/// Parses the server parameters for Apple security type 30.
///
/// The wire message is a two-byte generator, a two-byte key length, then a
/// prime modulus and server public key of that length. This is a distinct ARD
/// authentication exchange, not VNC challenge-response authentication.
pub fn parse_ard_auth_challenge(
    bytes: &[u8],
    max_key_bytes: usize,
) -> Result<(ArdAuthChallenge, usize)> {
    let mut cursor = Cursor::new(bytes);
    let generator = cursor.u16()?;
    let key_length = usize::from(cursor.u16()?);
    if key_length == 0 {
        return Err(Error::Invalid("empty ARD authentication key"));
    }
    if key_length > max_key_bytes {
        return Err(Error::LimitExceeded("ARD authentication key"));
    }
    let prime = cursor.take(key_length)?.to_vec();
    let server_public_key = cursor.take(key_length)?.to_vec();
    if generator < 2 {
        return Err(Error::Invalid("invalid ARD authentication generator"));
    }
    if prime.iter().all(|&byte| byte == 0) || server_public_key.iter().all(|&byte| byte == 0) {
        return Err(Error::Invalid("invalid ARD authentication parameter"));
    }
    Ok((
        ArdAuthChallenge {
            generator,
            prime,
            server_public_key,
        },
        cursor.position(),
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArdAuthResponse {
    pub encrypted_credentials: [u8; 128],
    pub client_public_key: Vec<u8>,
}

/// Parses the client response for Apple security type 30.
pub fn parse_ard_auth_response(
    bytes: &[u8],
    key_length: usize,
    max_key_bytes: usize,
) -> Result<(ArdAuthResponse, usize)> {
    if key_length == 0 {
        return Err(Error::Invalid("empty ARD authentication key"));
    }
    if key_length > max_key_bytes {
        return Err(Error::LimitExceeded("ARD authentication key"));
    }
    let mut cursor = Cursor::new(bytes);
    let encrypted_credentials = cursor
        .take(128)?
        .try_into()
        .expect("fixed-size credential block");
    let client_public_key = cursor.take(key_length)?.to_vec();
    if client_public_key.iter().all(|&byte| byte == 0) {
        return Err(Error::Invalid("invalid ARD client public key"));
    }
    Ok((
        ArdAuthResponse {
            encrypted_credentials,
            client_public_key,
        },
        cursor.position(),
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArdClientInit {
    /// Raw Apple client-initialization flags. Apple Screen Sharing uses values
    /// beyond the standard RFB boolean; `0xc1` was observed on macOS 26.
    pub flags: u8,
}

impl ArdClientInit {
    pub fn shared(self) -> bool {
        self.flags != 0
    }
}

pub fn parse_ard_client_init(bytes: &[u8]) -> Result<(ArdClientInit, usize)> {
    let flags = *bytes.first().ok_or(Error::NeedMore {
        needed: 1,
        available: 0,
    })?;
    Ok((ArdClientInit { flags }, 1))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArdSessionOptions {
    pub flags: u8,
}

/// Parses Apple client message 10 (`10 00 00 flags`), sent before the first
/// SetEncodings/FramebufferUpdateRequest sequence by current Screen Sharing.
pub fn parse_ard_session_options(bytes: &[u8]) -> Result<(ArdSessionOptions, usize)> {
    let mut cursor = Cursor::new(bytes);
    if cursor.u8()? != 10 {
        return Err(Error::Invalid("not an ARD session-options message"));
    }
    if cursor.u8()? != 0 || cursor.u8()? != 0 {
        return Err(Error::Invalid("invalid ARD session-options padding"));
    }
    Ok((
        ArdSessionOptions {
            flags: cursor.u8()?,
        },
        cursor.position(),
    ))
}

/// Apple client message `0x12`, named `RFBSetEncryptionLevel` by the installed
/// Screen Sharing framework.
///
/// Command 1 carries the requested encryption level and a list of supported
/// big-endian 32-bit encryption-method identifiers. The installed client
/// sends `12 00 00 01 00 01 00 01 00 00 00 01`: command 1, level 1, one
/// method, method `1` (ComCryption). The screensharingd handler rejects
/// counts of 101 or more and accepts method `1`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArdSetEncryptionLevel {
    pub command: u16,
    pub level: u16,
    pub methods: Vec<u32>,
}

impl ArdSetEncryptionLevel {
    pub const MESSAGE_TYPE: u8 = 0x12;
    pub const COMMAND_SET_METHODS: u16 = 1;
    pub const COMMAND_ACTIVATE: u16 = 2;
    pub const MAX_METHOD_COUNT: usize = 100;

    pub fn activation() -> Self {
        Self {
            command: Self::COMMAND_ACTIVATE,
            level: 1,
            methods: Vec::new(),
        }
    }
}

/// Builds the 12-byte `RFBSetEncryptionLevel` proposal used by the installed
/// client: type `0x12`, command 1, a big-endian level, a big-endian method
/// count, then that many big-endian method identifiers.
pub fn build_ard_set_encryption_level(level: u16, methods: &[u32]) -> Result<Vec<u8>> {
    if level > 1 {
        return Err(Error::Invalid("unsupported ARD encryption level"));
    }
    if methods.len() > ArdSetEncryptionLevel::MAX_METHOD_COUNT {
        return Err(Error::LimitExceeded("ARD encryption method count"));
    }
    let capacity = 8_usize
        .checked_add(
            methods
                .len()
                .checked_mul(4)
                .ok_or(Error::LimitExceeded("ARD encryption method count"))?,
        )
        .ok_or(Error::LimitExceeded("ARD encryption method count"))?;
    let mut out = Vec::with_capacity(capacity);
    out.extend_from_slice(&[ArdSetEncryptionLevel::MESSAGE_TYPE, 0]);
    out.extend_from_slice(&ArdSetEncryptionLevel::COMMAND_SET_METHODS.to_be_bytes());
    out.extend_from_slice(&level.to_be_bytes());
    out.extend_from_slice(
        &u16::try_from(methods.len())
            .map_err(|_| Error::LimitExceeded("ARD encryption method count"))?
            .to_be_bytes(),
    );
    for method in methods {
        out.extend_from_slice(&method.to_be_bytes());
    }
    Ok(out)
}

/// Builds the 8-byte activation message the client sends after accepting the
/// 1103 encryption-control rectangle: `12 00 00 02 00 01 00 00`. The
/// screensharingd `HandleSetEncryptionMessage` treats this as the transition
/// to decrypt everything received from the client.
pub fn build_ard_encryption_activation() -> [u8; 8] {
    [
        ArdSetEncryptionLevel::MESSAGE_TYPE,
        0,
        0,
        ArdSetEncryptionLevel::COMMAND_ACTIVATE as u8,
        0,
        1,
        0,
        0,
    ]
}

/// Parses a client `0x12` message with the same bounded semantics as the
/// installed `HandleSetEncryptionMessage` handler. Command 1 requires the
/// declared method list to be present and bounded; command 2 is fixed at
/// eight bytes.
pub fn parse_ard_set_encryption_level(
    bytes: &[u8],
    max_methods: usize,
) -> Result<(ArdSetEncryptionLevel, usize)> {
    let mut cursor = Cursor::new(bytes);
    if cursor.u8()? != ArdSetEncryptionLevel::MESSAGE_TYPE {
        return Err(Error::Invalid("not an ARD set-encryption-level message"));
    }
    if cursor.u8()? != 0 {
        return Err(Error::Invalid("invalid ARD set-encryption-level padding"));
    }
    let command = cursor.u16()?;
    let level = cursor.u16()?;
    let count = usize::from(cursor.u16()?);
    if count > ArdSetEncryptionLevel::MAX_METHOD_COUNT {
        return Err(Error::LimitExceeded("ARD encryption method count"));
    }
    if count > max_methods {
        return Err(Error::LimitExceeded("ARD encryption method count"));
    }
    let mut methods = Vec::with_capacity(count);
    match command {
        ArdSetEncryptionLevel::COMMAND_SET_METHODS => {
            for _ in 0..count {
                methods.push(cursor.u32()?);
            }
        }
        ArdSetEncryptionLevel::COMMAND_ACTIVATE => {
            if count != 0 {
                return Err(Error::Invalid(
                    "ARD encryption activation has a nonzero method count",
                ));
            }
        }
        _ => return Err(Error::Invalid("unsupported ARD encryption command")),
    }
    Ok((
        ArdSetEncryptionLevel {
            command,
            level,
            methods,
        },
        cursor.position(),
    ))
}

/// Apple client message `0x21`, named `RFBViewerInformation` by the installed
/// Screen Sharing framework.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArdViewerInformation {
    pub version: u16,
    /// Four viewer-supplied components whose exact semantics are not yet
    /// established. Current Screen Sharing sends `[2, 6, 1, 0]`.
    pub viewer_components: [u32; 4],
    /// macOS major, minor, and patch components.
    pub system_version: [u32; 3],
    /// Capability/reserved bytes. They are retained without assigning
    /// unconfirmed bit meanings.
    pub capabilities: [u8; 32],
}

impl ArdViewerInformation {
    pub const MESSAGE_TYPE: u8 = 0x21;
    pub const VERSION: u16 = 1;
    pub const PAYLOAD_LEN: usize = 62;
    pub const WIRE_LEN: usize = 66;
}

pub fn parse_ard_viewer_information(
    bytes: &[u8],
    max_message_bytes: usize,
) -> Result<(ArdViewerInformation, usize)> {
    let mut cursor = Cursor::new(bytes);
    if cursor.u8()? != ArdViewerInformation::MESSAGE_TYPE {
        return Err(Error::Invalid("not an RFBViewerInformation message"));
    }
    if cursor.u8()? != 0 {
        return Err(Error::Invalid("invalid RFBViewerInformation padding"));
    }
    let payload_len = usize::from(cursor.u16()?);
    let wire_len = payload_len
        .checked_add(4)
        .ok_or(Error::LimitExceeded("RFBViewerInformation message"))?;
    if wire_len > max_message_bytes {
        return Err(Error::LimitExceeded("RFBViewerInformation message"));
    }
    if payload_len != ArdViewerInformation::PAYLOAD_LEN {
        return Err(Error::Invalid("unsupported RFBViewerInformation length"));
    }
    let payload = cursor.take(payload_len)?;
    let mut payload = Cursor::new(payload);
    let version = payload.u16()?;
    if version != ArdViewerInformation::VERSION {
        return Err(Error::Invalid("unsupported RFBViewerInformation version"));
    }
    let mut viewer_components = [0; 4];
    for component in &mut viewer_components {
        *component = payload.u32()?;
    }
    let mut system_version = [0; 3];
    for component in &mut system_version {
        *component = payload.u32()?;
    }
    let capabilities = payload
        .take(32)?
        .try_into()
        .expect("fixed viewer-information capability size");
    debug_assert_eq!(payload.remaining(), 0);
    Ok((
        ArdViewerInformation {
            version,
            viewer_components,
            system_version,
            capabilities,
        },
        cursor.position(),
    ))
}

/// The fixed payload of the zero-sized framebuffer rectangle whose encoding
/// is 1103. The installed client calls its handler `HandleEncryptionEncoding`.
#[derive(Clone, PartialEq, Eq)]
pub struct ArdEncryptionControl {
    pub command: u32,
    wrapped_session_blocks: [[u8; 16]; 2],
}

impl ArdEncryptionControl {
    pub const ENABLE_COMMAND: u32 = 1;
    pub const WIRE_LEN: usize = 36;

    pub fn new(command: u32, wrapped_session_blocks: [[u8; 16]; 2]) -> Result<Self> {
        if command != Self::ENABLE_COMMAND {
            return Err(Error::Invalid("unsupported ARD encryption command"));
        }
        Ok(Self {
            command,
            wrapped_session_blocks,
        })
    }

    pub fn wrapped_session_blocks(&self) -> &[[u8; 16]; 2] {
        &self.wrapped_session_blocks
    }
}

impl core::fmt::Debug for ArdEncryptionControl {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("ArdEncryptionControl")
            .field("command", &self.command)
            .field("wrapped_session_blocks", &"<redacted>")
            .finish()
    }
}

pub fn parse_ard_encryption_control(bytes: &[u8]) -> Result<(ArdEncryptionControl, usize)> {
    let mut cursor = Cursor::new(bytes);
    let command = cursor.u32()?;
    if command != ArdEncryptionControl::ENABLE_COMMAND {
        return Err(Error::Invalid("unsupported ARD encryption command"));
    }
    let first = cursor
        .take(16)?
        .try_into()
        .expect("fixed encrypted key block");
    let second = cursor
        .take(16)?
        .try_into()
        .expect("fixed encrypted key block");
    Ok((
        ArdEncryptionControl {
            command,
            wrapped_session_blocks: [first, second],
        },
        cursor.position(),
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelFormat {
    pub bits_per_pixel: u8,
    pub depth: u8,
    pub big_endian: bool,
    pub true_color: bool,
    pub red_max: u16,
    pub green_max: u16,
    pub blue_max: u16,
    pub red_shift: u8,
    pub green_shift: u8,
    pub blue_shift: u8,
}

impl PixelFormat {
    pub const XRGB8888: Self = Self {
        bits_per_pixel: 32,
        depth: 24,
        big_endian: false,
        true_color: true,
        red_max: 255,
        green_max: 255,
        blue_max: 255,
        red_shift: 16,
        green_shift: 8,
        blue_shift: 0,
    };

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let mut cursor = Cursor::new(bytes);
        let value = Self {
            bits_per_pixel: cursor.u8()?,
            depth: cursor.u8()?,
            big_endian: cursor.u8()? != 0,
            true_color: cursor.u8()? != 0,
            red_max: cursor.u16()?,
            green_max: cursor.u16()?,
            blue_max: cursor.u16()?,
            red_shift: cursor.u8()?,
            green_shift: cursor.u8()?,
            blue_shift: cursor.u8()?,
        };
        cursor.take(3)?;
        value.validate()?;
        Ok(value)
    }

    pub fn encode(self) -> Result<[u8; 16]> {
        self.validate()?;
        let mut out = [0; 16];
        out[0] = self.bits_per_pixel;
        out[1] = self.depth;
        out[2] = u8::from(self.big_endian);
        out[3] = u8::from(self.true_color);
        out[4..6].copy_from_slice(&self.red_max.to_be_bytes());
        out[6..8].copy_from_slice(&self.green_max.to_be_bytes());
        out[8..10].copy_from_slice(&self.blue_max.to_be_bytes());
        out[10] = self.red_shift;
        out[11] = self.green_shift;
        out[12] = self.blue_shift;
        Ok(out)
    }

    pub fn bytes_per_pixel(self) -> Result<usize> {
        match self.bits_per_pixel {
            8 | 16 | 32 => Ok(usize::from(self.bits_per_pixel / 8)),
            _ => Err(Error::Invalid("unsupported bits-per-pixel")),
        }
    }

    pub(crate) fn validate(self) -> Result<()> {
        let _ = self.bytes_per_pixel()?;
        if !self.true_color {
            return Err(Error::Invalid("colour-map pixel formats are not supported"));
        }
        if self.depth == 0 || self.depth > self.bits_per_pixel {
            return Err(Error::Invalid("invalid pixel depth"));
        }
        for (max, shift) in [
            (self.red_max, self.red_shift),
            (self.green_max, self.green_shift),
            (self.blue_max, self.blue_shift),
        ] {
            if max == 0 || (u64::from(max) << shift) >= (1_u64 << self.bits_per_pixel) {
                return Err(Error::Invalid("invalid true-colour channel"));
            }
        }
        Ok(())
    }

    pub(crate) fn decode_pixel(self, bytes: &[u8]) -> Result<[u8; 4]> {
        let bpp = self.bytes_per_pixel()?;
        if bytes.len() < bpp {
            return Err(Error::NeedMore {
                needed: bpp,
                available: bytes.len(),
            });
        }
        let value = match (bpp, self.big_endian) {
            (1, _) => u32::from(bytes[0]),
            (2, true) => u32::from(u16::from_be_bytes([bytes[0], bytes[1]])),
            (2, false) => u32::from(u16::from_le_bytes([bytes[0], bytes[1]])),
            (4, true) => u32::from_be_bytes(bytes[..4].try_into().expect("length checked")),
            (4, false) => u32::from_le_bytes(bytes[..4].try_into().expect("length checked")),
            _ => unreachable!(),
        };
        Ok([
            scale_channel(
                (value >> self.red_shift) & u32::from(self.red_max),
                self.red_max,
            ),
            scale_channel(
                (value >> self.green_shift) & u32::from(self.green_max),
                self.green_max,
            ),
            scale_channel(
                (value >> self.blue_shift) & u32::from(self.blue_max),
                self.blue_max,
            ),
            255,
        ])
    }

    pub(crate) fn encode_pixel(self, rgba: [u8; 4], bytes: &mut [u8]) -> Result<()> {
        let bpp = self.bytes_per_pixel()?;
        if bytes.len() < bpp {
            return Err(Error::NeedMore {
                needed: bpp,
                available: bytes.len(),
            });
        }
        let scale = |value: u8, max: u16| (u32::from(value) * u32::from(max) + 127) / 255;
        let value = (scale(rgba[0], self.red_max) << self.red_shift)
            | (scale(rgba[1], self.green_max) << self.green_shift)
            | (scale(rgba[2], self.blue_max) << self.blue_shift);
        match (bpp, self.big_endian) {
            (1, _) => bytes[0] = value as u8,
            (2, true) => bytes[..2].copy_from_slice(&(value as u16).to_be_bytes()),
            (2, false) => bytes[..2].copy_from_slice(&(value as u16).to_le_bytes()),
            (4, true) => bytes[..4].copy_from_slice(&value.to_be_bytes()),
            (4, false) => bytes[..4].copy_from_slice(&value.to_le_bytes()),
            _ => unreachable!(),
        }
        Ok(())
    }
}

fn scale_channel(value: u32, max: u16) -> u8 {
    ((value * 255 + u32::from(max) / 2) / u32::from(max)) as u8
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInit {
    pub width: u16,
    pub height: u16,
    pub pixel_format: PixelFormat,
    pub name: String,
    /// Apple ARD ServerInit extension. Present when the server advertises
    /// extended command support through the extra block that screensharingd's
    /// `SendServerInitialiation` appends after the standard 24-byte header.
    pub extension: Option<ArdServerInitExtension>,
}

/// The Apple extension appended to `ServerInit` when the server name field
/// carries at least 22 bytes: a zero u16, a big-endian flags word, a 16-byte
/// command-support bitfield, then the machine name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArdServerInitExtension {
    pub flags: u32,
    /// One support bit per command, MSB-first within each byte, matching the
    /// client's `RFBServerCommandSupported` lookup.
    pub command_support: [u8; 16],
}

impl ArdServerInitExtension {
    pub fn supports_command(&self, command: u8) -> bool {
        let byte = usize::from(command) / 8;
        let bit = usize::from(command) % 8;
        byte < 16 && self.command_support[byte] & (0x80 >> bit) != 0
    }
}

pub fn parse_server_init(bytes: &[u8], max_name_len: usize) -> Result<(ServerInit, usize)> {
    let mut cursor = Cursor::new(bytes);
    let width = cursor.u16()?;
    let height = cursor.u16()?;
    let pixel_format = PixelFormat::parse(cursor.take(16)?)?;
    let payload_len =
        usize::try_from(cursor.u32()?).map_err(|_| Error::LimitExceeded("server name length"))?;
    if payload_len > max_name_len {
        return Err(Error::LimitExceeded("server name length"));
    }
    let payload = cursor.take(payload_len)?;
    let (name, extension) = parse_server_init_payload(payload)?;
    Ok((
        ServerInit {
            width,
            height,
            pixel_format,
            name,
            extension,
        },
        cursor.position(),
    ))
}

fn parse_server_init_payload(payload: &[u8]) -> Result<(String, Option<ArdServerInitExtension>)> {
    if payload.len() >= 22 && payload[..2] == [0, 0] {
        let flags = u32::from_be_bytes(
            payload[2..6]
                .try_into()
                .expect("extension flags length checked"),
        );
        let command_support = payload[6..22]
            .try_into()
            .expect("extension bitfield length checked");
        let name = core::str::from_utf8(&payload[22..])
            .map_err(|_| Error::Invalid("server name is not UTF-8"))?
            .to_owned();
        Ok((
            name,
            Some(ArdServerInitExtension {
                flags,
                command_support,
            }),
        ))
    } else {
        let name = core::str::from_utf8(payload)
            .map_err(|_| Error::Invalid("server name is not UTF-8"))?
            .to_owned();
        Ok((name, None))
    }
}

/// Builds the Apple extended ServerInit used by `screensharingd`: the
/// standard 24-byte header, then a payload of 22 extension bytes plus the
/// machine name. The extension advertises the given command-support
/// bitfield (MSB-first) so the native client enables `0x12` encryption.
pub fn build_ard_server_init(
    width: u16,
    height: u16,
    pixel_format: PixelFormat,
    name: &[u8],
    flags: u32,
    command_support: [u8; 16],
) -> Result<Vec<u8>> {
    let payload_len = 22_usize
        .checked_add(name.len())
        .ok_or(Error::LimitExceeded("server name length"))?;
    let mut out = Vec::with_capacity(
        24_usize
            .checked_add(payload_len)
            .ok_or(Error::LimitExceeded("server name length"))?,
    );
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&pixel_format.encode()?);
    out.extend_from_slice(
        &u32::try_from(payload_len)
            .map_err(|_| Error::LimitExceeded("server name length"))?
            .to_be_bytes(),
    );
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&flags.to_be_bytes());
    out.extend_from_slice(&command_support);
    out.extend_from_slice(name);
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum Encoding {
    Raw = 0,
    CopyRect = 1,
    Zlib = 6,
    Zrle = 16,
    /// Apple cursor-position rectangle. The native client treats it as an
    /// ordinary FramebufferUpdate rectangle: the position lives in the
    /// rectangle header and the payload is empty. The server emits it when
    /// the pointer moves, even outside the shared framebuffer.
    CursorPosition = 1100,
    ArdHalftone = 1000,
    ArdGrayscale = 1001,
    ArdThousands = 1002,
    /// Apple AVC media-stream bootstrap rectangle. The rectangle payload is
    /// a `RFBMediaStreamMessage1` and the actual video arrives over UDP.
    ArdAvcMediaStream = 1010,
    ArdMvs = 1011,
    ArdEncryption = 1103,
    DesktopSize = -223,
}

impl Encoding {
    pub fn from_i32(value: i32) -> Option<Self> {
        Some(match value {
            0 => Self::Raw,
            1 => Self::CopyRect,
            6 => Self::Zlib,
            16 => Self::Zrle,
            1100 => Self::CursorPosition,
            1000 => Self::ArdHalftone,
            1001 => Self::ArdGrayscale,
            1002 => Self::ArdThousands,
            1010 => Self::ArdAvcMediaStream,
            1011 => Self::ArdMvs,
            1103 => Self::ArdEncryption,
            -223 => Self::DesktopSize,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rectangle {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub encoding: i32,
}

pub fn build_set_pixel_format(format: PixelFormat) -> Result<[u8; 20]> {
    let mut out = [0; 20];
    out[0] = 0;
    out[4..].copy_from_slice(&format.encode()?);
    Ok(out)
}

pub fn build_set_encodings(encodings: &[i32]) -> Result<Vec<u8>> {
    let count =
        u16::try_from(encodings.len()).map_err(|_| Error::LimitExceeded("encoding count"))?;
    let capacity = 4_usize
        .checked_add(encodings.len().saturating_mul(4))
        .ok_or(Error::LimitExceeded("encoding message size"))?;
    let mut out = Vec::with_capacity(capacity);
    out.extend_from_slice(&[2, 0]);
    out.extend_from_slice(&count.to_be_bytes());
    for encoding in encodings {
        out.extend_from_slice(&encoding.to_be_bytes());
    }
    Ok(out)
}

/// Builds the standard RFB client `KeyEvent` message.
///
/// The keysym is an X11/RFB keysym, not a platform scan code. Keeping that
/// distinction at the protocol boundary is what lets the viewer use the same
/// wire representation on macOS, Windows, and Linux.
pub fn build_key_event(pressed: bool, keysym: u32) -> [u8; 8] {
    let mut out = [0; 8];
    out[0] = 4;
    out[1] = u8::from(pressed);
    out[4..].copy_from_slice(&keysym.to_be_bytes());
    out
}

/// Builds the standard RFB client `PointerEvent` message.
///
/// The button mask is serialized without reinterpretation. Standard RFB uses
/// bit 0/1/2 for left/middle/right, while Apple's ARD implementation uses
/// left/right/middle; callers must supply the server's expected ordering.
pub fn build_pointer_event(button_mask: u8, x: u16, y: u16) -> [u8; 6] {
    let mut out = [0; 6];
    out[0] = 5;
    out[1] = button_mask;
    out[2..4].copy_from_slice(&x.to_be_bytes());
    out[4..6].copy_from_slice(&y.to_be_bytes());
    out
}

/// The native macOS scroll-wheel payload carried by Apple's extended input
/// message. Unlike the standard RFB wheel buttons, it preserves precise point
/// deltas and the scroll/momentum phases used by trackpads.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ArdScrollWheelEvent {
    pub delta_x: i16,
    pub delta_y: i16,
    pub delta_z: i16,
    pub fixed_delta_x: i32,
    pub fixed_delta_y: i32,
    pub fixed_delta_z: i32,
    pub point_delta_x: i32,
    pub point_delta_y: i32,
    pub point_delta_z: i32,
    pub scroll_phase: u32,
    pub momentum_phase: u32,
    pub scroll_count: u32,
    pub flags: u32,
    pub x: u16,
    pub y: u16,
}

/// Builds Apple's `RFBPostScrollWheelEvent` client message (`0x17/0x0036`).
///
/// The layout is byte-for-byte compatible with the message emitted by the
/// ScreenSharing private framework on macOS 26.
pub fn build_ard_scroll_wheel_event(event: ArdScrollWheelEvent) -> [u8; 58] {
    let mut out = [0_u8; 58];
    out[0] = 0x17;
    out[2..4].copy_from_slice(&0x0036_u16.to_be_bytes());
    out[4..6].copy_from_slice(&1_u16.to_be_bytes());
    out[6..8].copy_from_slice(&11_u16.to_be_bytes());
    out[8..10].copy_from_slice(&event.delta_x.to_be_bytes());
    out[10..12].copy_from_slice(&event.delta_y.to_be_bytes());
    out[12..14].copy_from_slice(&event.delta_z.to_be_bytes());
    for (offset, value) in [
        (14, event.fixed_delta_x),
        (18, event.fixed_delta_y),
        (22, event.fixed_delta_z),
        (26, event.point_delta_x),
        (30, event.point_delta_y),
        (34, event.point_delta_z),
    ] {
        out[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
    }
    for (offset, value) in [
        (38, event.scroll_phase),
        (42, event.momentum_phase),
        (46, event.scroll_count),
        (50, event.flags),
    ] {
        out[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
    }
    out[54..56].copy_from_slice(&event.x.to_be_bytes());
    out[56..58].copy_from_slice(&event.y.to_be_bytes());
    out
}

/// Builds the standard RFB client `ClientCutText` clipboard message.
pub fn build_client_cut_text(text: &[u8]) -> Result<Vec<u8>> {
    let length = u32::try_from(text.len()).map_err(|_| Error::LimitExceeded("clipboard text"))?;
    let capacity = 8_usize
        .checked_add(text.len())
        .ok_or(Error::LimitExceeded("clipboard text"))?;
    let mut out = Vec::with_capacity(capacity);
    out.extend_from_slice(&[6, 0, 0, 0]);
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(text);
    Ok(out)
}

/// Builds a UTF-8 RFB clipboard message.
pub fn build_clipboard_text(text: &str) -> Result<Vec<u8>> {
    build_client_cut_text(text.as_bytes())
}

pub fn build_framebuffer_update_request(
    incremental: bool,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
) -> [u8; 10] {
    let mut out = [0; 10];
    out[0] = 3;
    out[1] = u8::from(incremental);
    out[2..4].copy_from_slice(&x.to_be_bytes());
    out[4..6].copy_from_slice(&y.to_be_bytes());
    out[6..8].copy_from_slice(&width.to_be_bytes());
    out[8..10].copy_from_slice(&height.to_be_bytes());
    out
}

/// Builds Apple's 16-byte automatic framebuffer-update request.
///
/// The installed Screen Sharing client sends message type `9` with flag `1`,
/// a big-endian millisecond interval, and an optional update rectangle. An
/// interval of zero is the native default and lets the server deliver updates
/// as soon as they are available instead of waiting for a request after every
/// decoded frame.
pub fn build_ard_auto_frame_update(
    interval_ms: u32,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
) -> [u8; 16] {
    let mut out = [0; 16];
    out[0] = 9;
    out[2..4].copy_from_slice(&1_u16.to_be_bytes());
    out[4..8].copy_from_slice(&interval_ms.to_be_bytes());
    out[8..10].copy_from_slice(&x.to_be_bytes());
    out[10..12].copy_from_slice(&y.to_be_bytes());
    out[12..14].copy_from_slice(&width.to_be_bytes());
    out[14..16].copy_from_slice(&height.to_be_bytes());
    out
}

pub fn parse_framebuffer_update(
    bytes: &[u8],
    decoder: &mut Decoder,
    framebuffer: &mut Framebuffer,
) -> Result<usize> {
    parse_framebuffer_update_impl(bytes, decoder, framebuffer, false)
}

pub(crate) fn parse_complete_framebuffer_update(
    bytes: &[u8],
    decoder: &mut Decoder,
    framebuffer: &mut Framebuffer,
) -> Result<usize> {
    parse_framebuffer_update_impl(bytes, decoder, framebuffer, true)
}

fn parse_framebuffer_update_impl(
    bytes: &[u8],
    decoder: &mut Decoder,
    framebuffer: &mut Framebuffer,
    complete: bool,
) -> Result<usize> {
    let mut cursor = Cursor::new(bytes);
    if cursor.u8()? != 0 {
        return Err(Error::Invalid("not a FramebufferUpdate message"));
    }
    cursor.u8()?;
    let count = usize::from(cursor.u16()?);
    if count > decoder.limits().max_rectangles {
        return Err(Error::LimitExceeded("rectangle count"));
    }
    for _ in 0..count {
        let rect = Rectangle {
            x: cursor.u16()?,
            y: cursor.u16()?,
            width: cursor.u16()?,
            height: cursor.u16()?,
            encoding: cursor.i32()?,
        };
        let consumed = if complete {
            decoder.decode_complete_rectangle(rect, cursor.tail(), framebuffer)?
        } else {
            decoder.decode_rectangle(rect, cursor.tail(), framebuffer)?
        };
        cursor.take(consumed)?;
    }
    Ok(cursor.position())
}

/// Finds the exact boundary of a framebuffer update without running any
/// stateful codec. The caller can therefore buffer fragmented records without
/// repeatedly cloning the accumulated frame or the MVS decoder state.
pub(crate) fn complete_framebuffer_update_len(bytes: &[u8], decoder: &Decoder) -> Result<usize> {
    let mut cursor = Cursor::new(bytes);
    if cursor.u8()? != 0 {
        return Err(Error::Invalid("not a FramebufferUpdate message"));
    }
    cursor.u8()?;
    let count = usize::from(cursor.u16()?);
    if count > decoder.limits().max_rectangles {
        return Err(Error::LimitExceeded("rectangle count"));
    }
    for _ in 0..count {
        let rect = Rectangle {
            x: cursor.u16()?,
            y: cursor.u16()?,
            width: cursor.u16()?,
            height: cursor.u16()?,
            encoding: cursor.i32()?,
        };
        let payload_len = decoder.complete_rectangle_payload_len(rect, cursor.tail())?;
        cursor.take(payload_len)?;
    }
    Ok(cursor.position())
}
