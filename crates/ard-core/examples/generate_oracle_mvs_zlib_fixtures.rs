//! Generate the synthetic MVS and persistent-zlib payload streams consumed by
//! the oracle. Each output record is exactly one length-prefixed RFB rectangle
//! payload; rectangle headers and FramebufferUpdate envelopes are not stored.

use std::{
    env,
    fs::{self, File},
    io::{self, BufWriter, Write},
    path::PathBuf,
};

use ard_rs::{Decoder, Encoding, Framebuffer, PixelFormat, Rectangle};
use flate2::{Compress, Compression, FlushCompress};

const WIDTH: usize = 1920;
const HEIGHT: usize = 1080;
const DISPLAY_FRAMES_PER_SECOND: usize = 60;
const DEFAULT_DURATION_SECONDS: usize = 5;
const TILE: usize = 8;

const PALETTE: [[u8; 3]; 8] = [
    [29, 78, 216],
    [0, 166, 214],
    [0, 178, 120],
    [245, 190, 35],
    [239, 112, 33],
    [218, 55, 88],
    [146, 65, 211],
    [65, 83, 118],
];

const DIGITS: [u8; 10] = [
    0b011_1111, 0b000_0110, 0b101_1011, 0b100_1111, 0b110_0110, 0b110_1101, 0b111_1101, 0b000_0111,
    0b111_1111, 0b110_1111,
];

fn main() -> io::Result<()> {
    let output_dir = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("crates/ard-core/examples/fixtures"));
    let duration_seconds = env::args()
        .nth(2)
        .map(|value| {
            value
                .parse::<usize>()
                .expect("duration in seconds must be an integer")
        })
        .unwrap_or(DEFAULT_DURATION_SECONDS);
    let frame_count = duration_seconds
        .checked_mul(DISPLAY_FRAMES_PER_SECOND)
        .expect("fixture duration is too large");
    fs::create_dir_all(&output_dir)?;

    let mvs_path = output_dir.join("oracle-diagonal-frames-1920x1080.mvs");
    let zlib_path = output_dir.join("oracle-diagonal-frames-1920x1080.zlib");
    let mut mvs_file = BufWriter::new(File::create(&mvs_path)?);
    let mut zlib_file = BufWriter::new(File::create(&zlib_path)?);
    let mut compressor = Compress::new(Compression::default(), true);

    let mut mvs_decoder = Decoder::new(PixelFormat::XRGB8888).map_err(io::Error::other)?;
    let mut zlib_decoder = Decoder::new(PixelFormat::XRGB8888).map_err(io::Error::other)?;
    let mut mvs_framebuffer =
        Framebuffer::new(WIDTH as u16, HEIGHT as u16).map_err(io::Error::other)?;
    let mut zlib_framebuffer =
        Framebuffer::new(WIDTH as u16, HEIGHT as u16).map_err(io::Error::other)?;
    let mut pixels = vec![[0_u8; 3]; WIDTH * HEIGHT];

    for frame_number in 1..=frame_count {
        paint_frame(&mut pixels, frame_number);
        let mvs_record = encode_mvs_record(&pixels);
        let zlib_record = encode_zlib_record(&mut compressor, &pixels)?;

        mvs_file.write_all(&mvs_record)?;
        zlib_file.write_all(&zlib_record)?;
        validate_record(
            &mut mvs_decoder,
            &mut mvs_framebuffer,
            Encoding::ArdMvs,
            &mvs_record,
            &pixels,
            true,
        )?;
        validate_record(
            &mut zlib_decoder,
            &mut zlib_framebuffer,
            Encoding::Zlib,
            &zlib_record,
            &pixels,
            false,
        )?;
    }

    mvs_file.flush()?;
    zlib_file.flush()?;
    println!(
        "generated {frame_count} validated frames ({duration_seconds}s): {}",
        mvs_path.display()
    );
    println!(
        "generated {frame_count} validated frames ({duration_seconds}s): {}",
        zlib_path.display()
    );
    Ok(())
}

fn paint_frame(pixels: &mut [[u8; 3]], frame_number: usize) {
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            pixels[y * WIDTH + x] = PALETTE[((x + y) / 112) % PALETTE.len()];
        }
    }

    const DIGIT_WIDTH: usize = 96;
    const DIGIT_HEIGHT: usize = 192;
    const THICKNESS: usize = 16;
    const GAP: usize = 32;
    const NUMBER_X: usize = 720;
    const NUMBER_Y: usize = 448;

    fill_rect(pixels, 664, 400, 592, 288, [10, 14, 24]);
    let digits = [
        (frame_number / 1000) % 10,
        (frame_number / 100) % 10,
        (frame_number / 10) % 10,
        frame_number % 10,
    ];
    for (position, digit) in digits.into_iter().enumerate() {
        let x = NUMBER_X + position * (DIGIT_WIDTH + GAP);
        paint_digit(
            pixels,
            x,
            NUMBER_Y,
            digit,
            DIGIT_WIDTH,
            DIGIT_HEIGHT,
            THICKNESS,
        );
    }
}

fn paint_digit(
    pixels: &mut [[u8; 3]],
    x: usize,
    y: usize,
    digit: usize,
    width: usize,
    height: usize,
    thickness: usize,
) {
    let half = height / 2;
    let horizontal_width = width - 2 * thickness;
    let vertical_height = half - thickness;
    let segments = [
        (x + thickness, y, horizontal_width, thickness),
        (
            x + width - thickness,
            y + thickness,
            thickness,
            vertical_height,
        ),
        (x + width - thickness, y + half, thickness, vertical_height),
        (
            x + thickness,
            y + height - thickness,
            horizontal_width,
            thickness,
        ),
        (x, y + half, thickness, vertical_height),
        (x, y + thickness, thickness, vertical_height),
        (
            x + thickness,
            y + half - thickness / 2,
            horizontal_width,
            thickness,
        ),
    ];
    for (bit, &(sx, sy, sw, sh)) in segments.iter().enumerate() {
        if DIGITS[digit] & (1 << bit) != 0 {
            fill_rect(pixels, sx + TILE, sy + TILE, sw, sh, [42, 49, 64]);
            fill_rect(pixels, sx, sy, sw, sh, [248, 250, 252]);
        }
    }
}

fn fill_rect(
    pixels: &mut [[u8; 3]],
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    color: [u8; 3],
) {
    debug_assert!(
        [x, y, width, height]
            .into_iter()
            .all(|value| value % TILE == 0)
    );
    for py in y..y + height {
        for px in x..x + width {
            pixels[py * WIDTH + px] = color;
        }
    }
}

fn encode_mvs_record(pixels: &[[u8; 3]]) -> Vec<u8> {
    let mut primary = BitWriter::default();
    let mut secondary = BitWriter::default();
    primary.push(0, 1); // initial state

    for tile_y in (0..HEIGHT).step_by(TILE) {
        for tile_x in (0..WIDTH).step_by(TILE) {
            primary.push(4, 3); // solid/two-colour tile
            primary.push(0, 1); // no repeat

            let mut colors = Vec::<[u8; 3]>::with_capacity(2);
            for y in 0..TILE {
                for x in 0..TILE {
                    let color = pixels[(tile_y + y) * WIDTH + tile_x + x];
                    if !colors.contains(&color) {
                        colors.push(color);
                    }
                }
            }
            assert!(colors.len() <= 2, "tile-aligned fixture must be bi-level");
            if colors.len() == 1 {
                secondary.push(0, 1); // solid tile
                secondary.push(0, 1); // replace remembered solid colour
                push_ycbcr20(&mut secondary, colors[0]);
            } else {
                secondary.push(1, 1); // two-colour tile
                secondary.push(0, 1); // replace both remembered colours
                push_ycbcr20(&mut secondary, colors[0]);
                push_ycbcr20(&mut secondary, colors[1]);

                let mut uniform_rows = 0_u8;
                let mut row_masks = Vec::new();
                for y in 0..TILE {
                    let mut mask = 0_u8;
                    for x in 0..TILE {
                        if pixels[(tile_y + y) * WIDTH + tile_x + x] == colors[0] {
                            mask |= 0x80 >> x;
                        }
                    }
                    if mask == 0xff {
                        uniform_rows |= 0x80 >> y;
                    } else {
                        row_masks.push(mask);
                    }
                }
                secondary.push(u32::from(uniform_rows), 8);
                for mask in row_masks {
                    secondary.push(u32::from(mask), 8);
                }
            }
        }
    }
    primary.push(0x6d, 8);
    secondary.push(0x6d, 8);

    let primary = primary.finish();
    let secondary = secondary.finish();
    let secondary_offset = 6 + primary.len();
    let mut payload = vec![
        0, // partial update
        0, // luma Rice parameter
        0, // chroma Rice parameter
        ((secondary_offset >> 16) & 0xff) as u8,
        ((secondary_offset >> 8) & 0xff) as u8,
        (secondary_offset & 0xff) as u8,
    ];
    payload.extend_from_slice(&primary);
    payload.extend_from_slice(&secondary);

    let mut record = Vec::with_capacity(4 + payload.len());
    record.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    record.extend_from_slice(&payload);
    record
}

fn encode_zlib_record(compressor: &mut Compress, pixels: &[[u8; 3]]) -> io::Result<Vec<u8>> {
    let mut plain = Vec::with_capacity(WIDTH * HEIGHT * 4);
    for &[red, green, blue] in pixels {
        plain.extend_from_slice(&[blue, green, red, 0]);
    }
    let before_in = compressor.total_in();
    let before_out = compressor.total_out();
    let mut compressed = vec![0_u8; plain.len() + 65_536];
    compressor
        .compress(&plain, &mut compressed, FlushCompress::Sync)
        .map_err(io::Error::other)?;
    if compressor.total_in() - before_in != plain.len() as u64 {
        return Err(io::Error::other(
            "zlib output buffer was unexpectedly too small",
        ));
    }
    compressed.truncate((compressor.total_out() - before_out) as usize);

    let mut record = Vec::with_capacity(4 + compressed.len());
    record.extend_from_slice(&(compressed.len() as u32).to_be_bytes());
    record.extend_from_slice(&compressed);
    Ok(record)
}

fn validate_record(
    decoder: &mut Decoder,
    framebuffer: &mut Framebuffer,
    encoding: Encoding,
    record: &[u8],
    source: &[[u8; 3]],
    mvs_lossy_color: bool,
) -> io::Result<()> {
    let consumed = decoder
        .decode_rectangle(
            Rectangle {
                x: 0,
                y: 0,
                width: WIDTH as u16,
                height: HEIGHT as u16,
                encoding: encoding as i32,
            },
            record,
            framebuffer,
        )
        .map_err(io::Error::other)?;
    if consumed != record.len() {
        return Err(io::Error::other(
            "decoder did not consume one complete record",
        ));
    }
    for (index, (actual, &rgb)) in framebuffer.pixels().chunks_exact(4).zip(source).enumerate() {
        let expected = if mvs_lossy_color {
            mvs_round_trip(rgb)
        } else {
            rgb
        };
        let expected_wire = [expected[2], expected[1], expected[0], 0];
        if actual != expected_wire {
            return Err(io::Error::other(format!(
                "decoded {encoding:?} pixel {index} mismatch: got {actual:?}, expected {expected_wire:?}",
            )));
        }
    }
    Ok(())
}

fn push_ycbcr20(bits: &mut BitWriter, rgb: [u8; 3]) {
    let [y, cb, cr] = rgb_to_mvs_ycbcr(rgb);
    bits.push(u32::from(y), 8);
    bits.push(u32::from(cb / 4), 6);
    bits.push(u32::from(cr / 4), 6);
}

fn rgb_to_mvs_ycbcr([red, green, blue]: [u8; 3]) -> [u8; 3] {
    let red = f64::from(red);
    let green = f64::from(green);
    let blue = f64::from(blue);
    let y = (0.299 * red + 0.587 * green + 0.114 * blue).round();
    let cb = (128.0 - 0.168_736 * red - 0.331_264 * green + 0.5 * blue).round();
    let cr = (128.0 + 0.5 * red - 0.418_688 * green - 0.081_312 * blue).round();
    [
        y.clamp(0.0, 255.0) as u8,
        ((cb.clamp(0.0, 255.0) as u8) / 4) * 4,
        ((cr.clamp(0.0, 255.0) as u8) / 4) * 4,
    ]
}

fn mvs_round_trip(rgb: [u8; 3]) -> [u8; 3] {
    let [y, cb, cr] = rgb_to_mvs_ycbcr(rgb);
    let y = i32::from(y);
    let cb = i32::from(cb) - 128;
    let cr = i32::from(cr) - 128;
    [
        (y + ((91_881 * cr + 32_768) >> 16)).clamp(0, 255) as u8,
        (y + ((32_768 - 22_554 * cb - 46_802 * cr) >> 16)).clamp(0, 255) as u8,
        (y + ((116_130 * cb + 32_768) >> 16)).clamp(0, 255) as u8,
    ]
}

#[derive(Default)]
struct BitWriter {
    bits: Vec<bool>,
}

impl BitWriter {
    fn push(&mut self, value: u32, width: u8) {
        for shift in (0..width).rev() {
            self.bits.push(value & (1 << shift) != 0);
        }
    }

    fn finish(self) -> Vec<u8> {
        let mut output = vec![0_u8; self.bits.len().div_ceil(8)];
        for (position, value) in self.bits.into_iter().enumerate() {
            if value {
                output[position / 8] |= 0x80 >> (position % 8);
            }
        }
        output
    }
}
