#![forbid(unsafe_code)]

use std::env;
use std::fs;
use std::path::PathBuf;

use ard_rs::{ArdMessageDispatcher, Decoder, Framebuffer, PixelFormat};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/native-mvs-white-64x64.ppm"));
    let capture = include_bytes!("../tests/fixtures/native-mvs-white-64x64.bin");

    let mut dispatcher = ArdMessageDispatcher::new(1024, 1024)?;
    let mut decoder = Decoder::new(PixelFormat::XRGB8888)?;
    let mut framebuffer = Framebuffer::new(64, 64)?;
    for fragment in capture.chunks(7) {
        dispatcher.push(fragment, &mut decoder, &mut framebuffer)?;
    }
    if dispatcher.buffered_bytes() != 0 {
        return Err("capture ended with an incomplete ARD message".into());
    }

    let mut ppm = format!(
        "P6\n{} {}\n255\n",
        framebuffer.width(),
        framebuffer.height()
    )
    .into_bytes();
    for pixel in framebuffer.rgba().chunks_exact(4) {
        ppm.extend_from_slice(&pixel[..3]);
    }
    fs::write(&output, ppm)?;
    println!(
        "decoded {} ARD bytes into {}x{} RGB image: {}",
        capture.len(),
        framebuffer.width(),
        framebuffer.height(),
        output.display()
    );
    Ok(())
}
