//! The pixel formats a media-stream video offer can advertise.
//!
//! The video settings inside the offer carry a `pixelFormats` bitmask that lists
//! every output format this client says it can decode. The server picks from
//! that menu: offering the full set gets HEVC Rext `yuv444p` for a screen, while
//! offering only the 4:2:0 entries gets HEVC Main `yuv420p`. The bit each fourcc
//! occupies was read out of the live
//! `+[VCMediaNegotiationBlobVideoSettings(VideoRules) storePixelFormatsInBitMap:]`
//! switch, and the properties in each variant's documentation come from
//! CoreVideo's own pixel-format description for that fourcc.

/// One output format this client claims it can decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MediaPixelFormat {
    /// `'420f'`, bit 0: 8-bit 4:2:0, full range.
    Nv12Full,
    /// `'420v'`, bit 1: 8-bit 4:2:0, video range.
    Nv12Video,
    /// `'x420'`, bit 2: 10-bit 4:2:0, video range.
    X420,
    /// `'444f'`, bit 3: 8-bit 4:4:4, full range.
    Yuv444Full,
    /// `'444v'`, bit 4: 8-bit 4:4:4, video range.
    Yuv444Video,
    /// `'xf44'`, bit 5: 10-bit 4:4:4, full range.
    Xf44,
    /// `'v0a8'`, bit 6: 8-bit 4:2:0 with a third (alpha) plane, video range.
    V0a8,
    /// `'s4as'`, bit 7: 16-bit 4:4:4 with a third plane, video range.
    S4as,
}

impl MediaPixelFormat {
    /// Every format the native client advertises, in bit order.
    pub const ALL: [Self; 8] = [
        Self::Nv12Full,
        Self::Nv12Video,
        Self::X420,
        Self::Yuv444Full,
        Self::Yuv444Video,
        Self::Xf44,
        Self::V0a8,
        Self::S4as,
    ];

    /// The fourcc as its big-endian ASCII value.
    pub const fn fourcc(self) -> u32 {
        match self {
            Self::Nv12Full => 0x3432_3066,
            Self::Nv12Video => 0x3432_3076,
            Self::X420 => 0x7834_3230,
            Self::Yuv444Full => 0x3434_3466,
            Self::Yuv444Video => 0x3434_3476,
            Self::Xf44 => 0x7866_3434,
            Self::V0a8 => 0x7630_6138,
            Self::S4as => 0x7334_6173,
        }
    }

    /// The fourcc as text, for logs and errors.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Nv12Full => "'420f'",
            Self::Nv12Video => "'420v'",
            Self::X420 => "'x420'",
            Self::Yuv444Full => "'444f'",
            Self::Yuv444Video => "'444v'",
            Self::Xf44 => "'xf44'",
            Self::V0a8 => "'v0a8'",
            Self::S4as => "'s4as'",
        }
    }

    /// The bit this format occupies in the offer's `pixelFormats` mask.
    pub const fn bit(self) -> u32 {
        match self {
            Self::Nv12Full => 0x01,
            Self::Nv12Video => 0x02,
            Self::X420 => 0x04,
            Self::Yuv444Full => 0x08,
            Self::Yuv444Video => 0x10,
            Self::Xf44 => 0x20,
            Self::V0a8 => 0x40,
            Self::S4as => 0x80,
        }
    }
}

/// A set of [`MediaPixelFormat`]s, as the offer's `pixelFormats` field carries it.
///
/// This is the client's half of the negotiation: the server chooses what to
/// encode from this menu, so narrowing it lowers the stream's chroma format and
/// widening it lets the server pick the richest one it can.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaPixelFormats(u32);

impl MediaPixelFormats {
    /// The format set the native client offers: every entry.
    pub const fn all() -> Self {
        Self(0xff)
    }

    /// A set from raw bits; bits beyond the eight known formats are dropped.
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits & 0xff)
    }

    /// The raw mask, as it goes on the wire.
    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn contains(self, format: MediaPixelFormat) -> bool {
        self.0 & format.bit() != 0
    }
}

#[cfg(test)]
mod tests {
    use super::{MediaPixelFormat, MediaPixelFormats};

    /// The bits are not a guess: they are the `orr` immediates of the native
    /// `storePixelFormatsInBitMap:` switch, in the order that method tests the
    /// fourccs.
    #[test]
    fn bits_match_the_native_bit_map() {
        for (index, format) in MediaPixelFormat::ALL.into_iter().enumerate() {
            assert_eq!(format.bit(), 1 << index, "{}", format.name());
        }
        assert_eq!(MediaPixelFormats::all().bits(), 0xff);
        for format in MediaPixelFormat::ALL {
            assert!(MediaPixelFormats::all().contains(format));
        }
    }

    /// Each variant must spell the fourcc the native method compares against.
    #[test]
    fn names_spell_their_fourcc() {
        for format in MediaPixelFormat::ALL {
            let bytes = format.fourcc().to_be_bytes();
            let text = core::str::from_utf8(&bytes).expect("fourcc is ASCII");
            assert_eq!(format!("'{text}'"), format.name());
        }
        assert_eq!(MediaPixelFormat::Nv12Full.fourcc(), 0x3432_3066);
        assert_eq!(MediaPixelFormat::S4as.fourcc(), 0x7334_6173);
    }

    /// A narrowed set keeps only the formats it names, which is how this client
    /// would stop claiming 4:4:4 support it does not use.
    #[test]
    fn a_narrowed_set_drops_the_other_formats() {
        let nv12_only = MediaPixelFormats::from_bits(
            MediaPixelFormat::Nv12Full.bit()
                | MediaPixelFormat::Nv12Video.bit()
                | MediaPixelFormat::X420.bit(),
        );
        assert_eq!(nv12_only.bits(), 0x07);
        assert!(nv12_only.contains(MediaPixelFormat::Nv12Full));
        assert!(!nv12_only.contains(MediaPixelFormat::Yuv444Full));
        assert!(!nv12_only.contains(MediaPixelFormat::S4as));
        // Bits outside the eight known formats are dropped rather than passed on.
        assert_eq!(MediaPixelFormats::from_bits(u32::MAX).bits(), 0xff);
    }
}
