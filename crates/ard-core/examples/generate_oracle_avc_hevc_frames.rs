//! Emit synthetic RGB24 frames for the AVC/HEVC oracle fixtures.
//!
//! This deliberately uses only the standard library so the fixture generator
//! does not depend on a particular FFmpeg build having the `drawtext` filter.

use std::{
    env,
    io::{self, BufWriter, Write},
};

const WIDTH: usize = 1920;
const HEIGHT: usize = 1080;
const SLICE_COUNT: usize = 4;
const DISPLAY_FRAMES_PER_SECOND: usize = 60;
// Native AVC aligns ceil(frame_height / 4) to a 16-pixel codec boundary.
const SLICE_HEIGHT: usize = 272;
const DEFAULT_DURATION_SECONDS: usize = 5;

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

// Seven-segment masks in the order: top, upper-right, lower-right, bottom,
// lower-left, upper-left, middle.
const DIGITS: [u8; 10] = [
    0b011_1111, 0b000_0110, 0b101_1011, 0b100_1111, 0b110_0110, 0b110_1101, 0b111_1101, 0b000_0111,
    0b111_1111, 0b110_1111,
];

fn main() -> io::Result<()> {
    let duration_seconds = env::args()
        .nth(1)
        .map(|value| {
            value
                .parse::<usize>()
                .expect("duration in seconds must be an integer")
        })
        .unwrap_or(DEFAULT_DURATION_SECONDS);
    let frames = duration_seconds
        .checked_mul(DISPLAY_FRAMES_PER_SECOND)
        .expect("fixture duration is too large");
    let stdout = io::stdout().lock();
    let mut output = BufWriter::with_capacity(WIDTH * SLICE_HEIGHT * 3, stdout);
    let mut frame = vec![0_u8; WIDTH * HEIGHT * 3];

    let black_row = vec![0_u8; WIDTH * 3];

    for index in 1..=frames {
        paint_background(&mut frame, index);
        paint_number_panel(&mut frame, index);
        // The live server feeds the four horizontal bands into one serial
        // codec prediction chain. Emit them in global decode order. The last
        // band is padded from 264 to 272 rows, exactly like the native layout.
        for slice_index in 0..SLICE_COUNT {
            let first_row = slice_index * SLICE_HEIGHT;
            for row in 0..SLICE_HEIGHT {
                let source_row = first_row + row;
                if source_row < HEIGHT {
                    let start = source_row * WIDTH * 3;
                    output.write_all(&frame[start..start + WIDTH * 3])?;
                } else {
                    output.write_all(&black_row)?;
                }
            }
        }
    }
    output.flush()
}

fn paint_background(frame: &mut [u8], _index: usize) {
    // Keep the requested background to one clean layer of equal-width,
    // solid-colour diagonal stripes. Only the centered frame number changes.
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let band = ((x + y) / 112) % PALETTE.len();
            let color = PALETTE[band];
            let offset = (y * WIDTH + x) * 3;
            frame[offset..offset + 3].copy_from_slice(&color);
        }
    }
}

fn paint_number_panel(frame: &mut [u8], index: usize) {
    const DIGIT_WIDTH: usize = 96;
    const DIGIT_HEIGHT: usize = 192;
    const THICKNESS: usize = 20;
    const GAP: usize = 26;
    const DIGIT_COUNT: usize = 4;
    const PANEL_PAD_X: usize = 54;
    const PANEL_PAD_Y: usize = 46;

    let number_width = DIGIT_COUNT * DIGIT_WIDTH + (DIGIT_COUNT - 1) * GAP;
    let number_x = (WIDTH - number_width) / 2;
    let number_y = (HEIGHT - DIGIT_HEIGHT) / 2;
    fill_rect(
        frame,
        number_x - PANEL_PAD_X,
        number_y - PANEL_PAD_Y,
        number_width + PANEL_PAD_X * 2,
        DIGIT_HEIGHT + PANEL_PAD_Y * 2,
        [10, 14, 24],
    );

    let digits = [
        (index / 1000) % 10,
        (index / 100) % 10,
        (index / 10) % 10,
        index % 10,
    ];
    for (position, digit) in digits.into_iter().enumerate() {
        let x = number_x + position * (DIGIT_WIDTH + GAP);
        paint_digit(
            frame,
            x,
            number_y,
            digit,
            DIGIT_WIDTH,
            DIGIT_HEIGHT,
            THICKNESS,
        );
    }
}

fn paint_digit(
    frame: &mut [u8],
    x: usize,
    y: usize,
    digit: usize,
    width: usize,
    height: usize,
    thickness: usize,
) {
    let mask = DIGITS[digit];
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
        if mask & (1 << bit) != 0 {
            // A dark offset keeps the number legible across all stripe colors.
            fill_rect(frame, sx + 7, sy + 7, sw, sh, [42, 49, 64]);
            fill_rect(frame, sx, sy, sw, sh, [248, 250, 252]);
        }
    }
}

fn fill_rect(frame: &mut [u8], x: usize, y: usize, width: usize, height: usize, color: [u8; 3]) {
    for py in y..(y + height).min(HEIGHT) {
        for px in x..(x + width).min(WIDTH) {
            let offset = (py * WIDTH + px) * 3;
            frame[offset..offset + 3].copy_from_slice(&color);
        }
    }
}
