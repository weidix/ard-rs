use core::fmt;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    NeedMore {
        needed: usize,
        available: usize,
    },
    Invalid(&'static str),
    UnsupportedServerMessage(u8),
    InvalidMvsDctCacheIndex {
        index: u16,
        reference: &'static str,
        tile_index: usize,
        bit_position: usize,
        entry_count: u32,
        write_index: u16,
        last_reference: u16,
    },
    LimitExceeded(&'static str),
    UnsupportedEncoding(i32),
    Decompression,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NeedMore { needed, available } => {
                write!(f, "need {needed} bytes, only {available} available")
            }
            Self::Invalid(message) => f.write_str(message),
            Self::UnsupportedServerMessage(message_type) => {
                write!(
                    f,
                    "unsupported ARD server message type 0x{message_type:02x}"
                )
            }
            Self::InvalidMvsDctCacheIndex {
                index,
                reference,
                tile_index,
                bit_position,
                entry_count,
                write_index,
                last_reference,
            } => write!(
                f,
                "invalid ARD MVS DCT cache index {index} ({reference}, tile {tile_index}, bit {bit_position}, entries {entry_count}, write {write_index}, last reference {last_reference})"
            ),
            Self::LimitExceeded(what) => write!(f, "{what} exceeds configured limit"),
            Self::UnsupportedEncoding(value) => write!(f, "unsupported encoding {value}"),
            Self::Decompression => f.write_str("invalid or truncated zlib stream"),
        }
    }
}

impl std::error::Error for Error {}
