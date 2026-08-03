#![forbid(unsafe_code)]

use std::env;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use ard_rs::{ArdMessageDispatcher, Decoder, Framebuffer, PixelFormat};

fn decode_hex_fixture(text: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    text.split_ascii_whitespace()
        .map(|octet| Ok(u8::from_str_radix(octet, 16)?))
        .collect()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/real-macos-mvs-256x256.ppm"));
    let capture = decode_hex_fixture(include_str!("../tests/fixtures/real-macos-mvs-256x256.hex"))?;

    let mut dispatcher = ArdMessageDispatcher::new(8 * 1024 * 1024, 1024)?;
    let mut decoder = Decoder::new(PixelFormat::XRGB8888)?;
    let mut framebuffer = Framebuffer::new(1920, 1080)?;
    for fragment in capture.chunks(257) {
        dispatcher.push(fragment, &mut decoder, &mut framebuffer)?;
    }
    if dispatcher.buffered_bytes() != 0 {
        return Err("capture ended with an incomplete ARD message".into());
    }

    let mut ppm = BufWriter::new(File::create(&output)?);
    ppm.write_all(b"P6\n256 256\n255\n")?;
    for y in 0..256 {
        for x in 0..256 {
            let offset = (y * usize::from(framebuffer.width()) + x) * 4;
            ppm.write_all(&framebuffer.rgba()[offset..offset + 3])?;
        }
    }
    ppm.flush()?;
    println!(
        "decoded {} real ARD bytes into 256x256 RGB image: {}",
        capture.len(),
        output.display()
    );
    Ok(())
}
