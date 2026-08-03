#![forbid(unsafe_code)]

//! Replays a decrypted ARD server-message stream without a live connection.

use std::env;
use std::error::Error as StdError;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;

use ard_rs::{ArdMessageDispatcher, Decoder, Framebuffer, PixelFormat};

fn main() -> Result<(), Box<dyn StdError>> {
    let mut args = env::args_os().skip(1);
    let input = args.next().map(PathBuf::from).ok_or_else(usage)?;
    let width = parse_dimension(args.next(), "width")?;
    let height = parse_dimension(args.next(), "height")?;
    let output = args.next().map(PathBuf::from).ok_or_else(usage)?;
    if args.next().is_some() {
        return Err(usage().into());
    }

    let capture = fs::read(&input)?;
    let mut dispatcher = ArdMessageDispatcher::new(64 * 1024 * 1024, 1024 * 1024)?;
    let mut decoder = Decoder::new(PixelFormat::XRGB8888)?;
    let mut framebuffer = Framebuffer::new(width, height)?;
    for fragment in capture.chunks(257) {
        dispatcher.push(fragment, &mut decoder, &mut framebuffer)?;
    }
    if dispatcher.buffered_bytes() != 0 {
        return Err(io::Error::other("capture ended with an incomplete ARD message").into());
    }
    if framebuffer
        .rgba()
        .chunks_exact(4)
        .any(|pixel| pixel[3] == 0)
    {
        return Err(io::Error::other("capture does not contain a complete framebuffer").into());
    }

    let mut ppm = BufWriter::new(File::create(&output)?);
    write!(ppm, "P6\n{width} {height}\n255\n")?;
    for pixel in framebuffer.rgba().chunks_exact(4) {
        ppm.write_all(&pixel[..3])?;
    }
    ppm.flush()?;
    println!(
        "decoded {} ARD bytes into {}x{} RGB image: {}",
        capture.len(),
        width,
        height,
        output.display()
    );
    Ok(())
}

fn parse_dimension(value: Option<std::ffi::OsString>, name: &str) -> io::Result<u16> {
    let value = value.ok_or_else(usage)?;
    let value = value
        .to_str()
        .ok_or_else(|| io::Error::other(format!("{name} is not UTF-8")))?;
    let value = value
        .parse::<u16>()
        .map_err(|_| io::Error::other(format!("invalid {name}")))?;
    if value == 0 {
        return Err(io::Error::other(format!("{name} must be nonzero")));
    }
    Ok(value)
}

fn usage() -> io::Error {
    io::Error::other("usage: decode_plaintext_capture INPUT WIDTH HEIGHT OUTPUT")
}
