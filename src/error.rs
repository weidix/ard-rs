use core::fmt;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    NeedMore { needed: usize, available: usize },
    Invalid(&'static str),
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
            Self::LimitExceeded(what) => write!(f, "{what} exceeds configured limit"),
            Self::UnsupportedEncoding(value) => write!(f, "unsupported encoding {value}"),
            Self::Decompression => f.write_str("invalid or truncated zlib stream"),
        }
    }
}

impl std::error::Error for Error {}
