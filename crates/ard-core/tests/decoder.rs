use ard_rs::{
    Decoder, Encoding, Error, Framebuffer, FramebufferFormat, PixelFormat, Rectangle,
    parse_ard_auth_challenge, parse_ard_auth_response, parse_ard_client_init,
    parse_ard_session_options, parse_framebuffer_update,
};
use flate2::{Compress, Compression, FlushCompress};

// The decoder tests compare colours for readability, while the core now
// exposes the negotiated XRGB8888 bytes directly.
trait TestRgba {
    fn rgba(&self) -> Vec<u8>;
}

impl TestRgba for Framebuffer {
    fn rgba(&self) -> Vec<u8> {
        let Some(format) = self.native_pixel_format() else {
            return Vec::new();
        };
        let Ok(bytes_per_pixel) = format.bytes_per_pixel() else {
            return Vec::new();
        };
        let scale =
            |value: u32, max: u16| (((value * 255) + u32::from(max) / 2) / u32::from(max)) as u8;
        self.pixels()
            .chunks_exact(bytes_per_pixel)
            .flat_map(|bytes| {
                let value = match (bytes_per_pixel, format.big_endian) {
                    (1, _) => u32::from(bytes[0]),
                    (2, true) => u32::from(u16::from_be_bytes([bytes[0], bytes[1]])),
                    (2, false) => u32::from(u16::from_le_bytes([bytes[0], bytes[1]])),
                    (4, true) => u32::from_be_bytes(bytes.try_into().unwrap()),
                    (4, false) => u32::from_le_bytes(bytes.try_into().unwrap()),
                    _ => return [0, 0, 0, 0],
                };
                [
                    scale(
                        (value >> format.red_shift) & u32::from(format.red_max),
                        format.red_max,
                    ),
                    scale(
                        (value >> format.green_shift) & u32::from(format.green_max),
                        format.green_max,
                    ),
                    scale(
                        (value >> format.blue_shift) & u32::from(format.blue_max),
                        format.blue_max,
                    ),
                    255,
                ]
            })
            .collect()
    }
}

fn rect(width: u16, height: u16, encoding: Encoding) -> Rectangle {
    Rectangle {
        x: 0,
        y: 0,
        width,
        height,
        encoding: encoding as i32,
    }
}

fn compressed_packet(stream: &mut Compress, plain: &[u8]) -> Vec<u8> {
    let mut compressed = vec![0; plain.len().saturating_mul(2).saturating_add(128)];
    let before_in = stream.total_in();
    let before_out = stream.total_out();
    stream
        .compress(plain, &mut compressed, FlushCompress::Sync)
        .unwrap();
    assert_eq!(stream.total_in() - before_in, plain.len() as u64);
    compressed.truncate((stream.total_out() - before_out) as usize);

    let mut packet = Vec::with_capacity(4 + compressed.len());
    packet.extend_from_slice(&(compressed.len() as u32).to_be_bytes());
    packet.extend_from_slice(&compressed);
    packet
}

fn packed_bits(fields: &[(u32, u8)]) -> Vec<u8> {
    let bit_count: usize = fields.iter().map(|(_, width)| usize::from(*width)).sum();
    let mut bytes = vec![0_u8; bit_count.div_ceil(8)];
    let mut position = 0_usize;
    for &(value, width) in fields {
        assert!(width <= 32);
        for shift in (0..width).rev() {
            if value & (1 << shift) != 0 {
                bytes[position / 8] |= 0x80 >> (position % 8);
            }
            position += 1;
        }
    }
    bytes
}

// Canonical encoder for the native luminance AC table used by full-update
// chroma records. This lets a test place a coefficient at an arbitrary
// zigzag position without hand-computing Huffman codes.
fn huffman_ac_symbol(symbol: u8) -> Vec<(u32, u8)> {
    const BITS: [u8; 17] = [
        0x00, 0x00, 0x02, 0x01, 0x03, 0x03, 0x02, 0x04, 0x03, 0x05, 0x05, 0x04, 0x04, 0x00, 0x00,
        0x01, 0x7d,
    ];
    const VALUES: [u8; 162] = [
        0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61,
        0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52,
        0xd1, 0xf0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25,
        0x26, 0x27, 0x28, 0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45,
        0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64,
        0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83,
        0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99,
        0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6,
        0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3,
        0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8,
        0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
    ];
    let mut code = 0_u32;
    let mut value_index = 0_usize;
    for (length, &count) in BITS.iter().enumerate().skip(1) {
        let count = usize::from(count);
        for offset in 0..count {
            if VALUES[value_index + offset] == symbol {
                return vec![(code + offset as u32, length as u8)];
            }
        }
        code = (code + count as u32) << 1;
        value_index += count;
    }
    panic!("Huffman symbol {symbol:#x} not found");
}

fn partial_mvs_packet(primary_fields: &[(u32, u8)]) -> Vec<u8> {
    partial_mvs_packet_with_secondary(primary_fields, &[])
}

fn partial_mvs_packet_with_secondary(
    primary_fields: &[(u32, u8)],
    secondary_fields: &[(u32, u8)],
) -> Vec<u8> {
    let primary = packed_bits(primary_fields);
    let mut secondary_fields = secondary_fields.to_vec();
    secondary_fields.push((0x6d, 8));
    let secondary = packed_bits(&secondary_fields);
    let secondary_offset = 6 + primary.len();
    let mut update = vec![
        0,
        0,
        0,
        ((secondary_offset >> 16) & 0xff) as u8,
        ((secondary_offset >> 8) & 0xff) as u8,
        (secondary_offset & 0xff) as u8,
    ];
    update.extend_from_slice(&primary);
    update.extend_from_slice(&secondary);
    let mut packet = (update.len() as u32).to_be_bytes().to_vec();
    packet.extend_from_slice(&update);
    packet
}

fn full_mvs_packet(header: [u8; 2], fields: &[(u32, u8)]) -> Vec<u8> {
    let mut fields = fields.to_vec();
    fields.extend_from_slice(&[(0x6d, 8), (0x76, 8), (0x73, 8)]);
    let mut update = vec![1, header[0], header[1]];
    update.extend_from_slice(&packed_bits(&fields));
    let mut packet = (update.len() as u32).to_be_bytes().to_vec();
    packet.extend_from_slice(&update);
    packet
}

#[test]
fn parses_apple_type_30_authentication_messages() {
    let mut challenge = vec![0, 2, 0, 4];
    challenge.extend_from_slice(&[0xf1, 0x23, 0x45, 0x67]);
    challenge.extend_from_slice(&[0x01, 0x02, 0x03, 0x04]);
    let (challenge, consumed) = parse_ard_auth_challenge(&challenge, 512).unwrap();
    assert_eq!(consumed, 12);
    assert_eq!(challenge.generator, 2);
    assert_eq!(challenge.prime, [0xf1, 0x23, 0x45, 0x67]);
    assert_eq!(challenge.server_public_key, [1, 2, 3, 4]);

    let mut response = vec![0xa5; 128];
    response.extend_from_slice(&[5, 6, 7, 8]);
    let (response, consumed) = parse_ard_auth_response(&response, 4, 512).unwrap();
    assert_eq!(consumed, 132);
    assert_eq!(response.encrypted_credentials, [0xa5; 128]);
    assert_eq!(response.client_public_key, [5, 6, 7, 8]);
}

#[test]
fn parses_live_apple_client_initialization_extensions() {
    let (client_init, consumed) = parse_ard_client_init(&[0xc1]).unwrap();
    assert_eq!(consumed, 1);
    assert_eq!(client_init.flags, 0xc1);
    assert!(client_init.shared());

    let (options, consumed) = parse_ard_session_options(&[10, 0, 0, 1]).unwrap();
    assert_eq!(consumed, 4);
    assert_eq!(options.flags, 1);
}

#[test]
fn bounds_apple_type_30_authentication_keys() {
    assert_eq!(
        parse_ard_auth_challenge(&[0, 2, 2, 0], 128).unwrap_err(),
        Error::LimitExceeded("ARD authentication key")
    );
    assert_eq!(
        parse_ard_auth_response(&[0; 128], 1024, 128).unwrap_err(),
        Error::LimitExceeded("ARD authentication key")
    );
}

#[test]
fn decodes_apple_halftone_and_preserves_stream_state() {
    let mut encoder = Compress::new(Compression::default(), true);
    let first = compressed_packet(&mut encoder, &[0b1010_0000]);
    let second = compressed_packet(&mut encoder, &[0b0101_0000]);
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(4, 2).unwrap();

    decoder
        .decode_rectangle(rect(4, 1, Encoding::ArdHalftone), &first, &mut framebuffer)
        .unwrap();
    decoder
        .decode_rectangle(
            Rectangle {
                y: 1,
                ..rect(4, 1, Encoding::ArdHalftone)
            },
            &second,
            &mut framebuffer,
        )
        .unwrap();

    let values: Vec<u8> = framebuffer.rgba().chunks_exact(4).map(|p| p[0]).collect();
    assert_eq!(values, [255, 0, 255, 0, 0, 255, 0, 255]);
}

#[test]
fn decodes_apple_four_bit_grayscale() {
    let mut encoder = Compress::new(Compression::default(), true);
    let packet = compressed_packet(&mut encoder, &[0x01, 0xef]);
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(4, 1).unwrap();
    decoder
        .decode_rectangle(
            rect(4, 1, Encoding::ArdGrayscale),
            &packet,
            &mut framebuffer,
        )
        .unwrap();
    let values: Vec<u8> = framebuffer.rgba().chunks_exact(4).map(|p| p[0]).collect();
    assert_eq!(values, [0, 16, 224, 255]);
}

#[test]
fn decodes_apple_rgb555_thousands_codec() {
    let pixels = [
        0x7c00_u16.to_be_bytes(),
        0x03e0_u16.to_be_bytes(),
        0x001f_u16.to_be_bytes(),
    ]
    .concat();
    let mut encoder = Compress::new(Compression::default(), true);
    let packet = compressed_packet(&mut encoder, &pixels);
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(3, 1).unwrap();
    decoder
        .decode_rectangle(
            rect(3, 1, Encoding::ArdThousands),
            &packet,
            &mut framebuffer,
        )
        .unwrap();
    assert_eq!(
        framebuffer.rgba(),
        &[255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255]
    );
}

#[test]
fn decodes_zrle_raw_tile_using_compact_pixels() {
    // ZRLE subencoding 0 followed by little-endian compact XRGB pixels (B,G,R).
    let zrle = [0, 0, 0, 255, 0, 255, 0];
    let mut encoder = Compress::new(Compression::default(), true);
    let packet = compressed_packet(&mut encoder, &zrle);
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(2, 1).unwrap();
    decoder
        .decode_rectangle(rect(2, 1, Encoding::Zrle), &packet, &mut framebuffer)
        .unwrap();
    assert_eq!(framebuffer.rgba(), &[255, 0, 0, 255, 0, 255, 0, 255]);
}

#[test]
fn parses_and_decodes_a_complete_framebuffer_update() {
    let mut encoder = Compress::new(Compression::default(), true);
    let payload = compressed_packet(&mut encoder, &[0xf0]);
    let mut message = vec![0, 0, 0, 1];
    message.extend_from_slice(&0_u16.to_be_bytes());
    message.extend_from_slice(&0_u16.to_be_bytes());
    message.extend_from_slice(&2_u16.to_be_bytes());
    message.extend_from_slice(&1_u16.to_be_bytes());
    message.extend_from_slice(&(Encoding::ArdHalftone as i32).to_be_bytes());
    message.extend_from_slice(&payload);

    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(2, 1).unwrap();
    assert_eq!(
        parse_framebuffer_update(&message, &mut decoder, &mut framebuffer).unwrap(),
        message.len()
    );
    assert_eq!(
        framebuffer.rgba(),
        &[255, 255, 255, 255, 255, 255, 255, 255]
    );
}

#[test]
fn rejects_out_of_bounds_rectangles_before_decoding() {
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(1, 1).unwrap();
    let error = decoder
        .decode_rectangle(rect(2, 1, Encoding::Raw), &[], &mut framebuffer)
        .unwrap_err();
    assert_eq!(
        error,
        Error::Invalid("rectangle is outside the framebuffer")
    );
}

#[test]
fn identifies_mvs_without_misparsing_it_as_vnc() {
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(1, 1).unwrap();
    let payload = [0, 0, 0, 1, 1];
    assert_eq!(
        decoder
            .decode_rectangle(rect(1, 1, Encoding::ArdMvs), &payload, &mut framebuffer)
            .unwrap_err(),
        Error::NeedMore {
            needed: 3,
            available: 1
        }
    );
}

#[test]
fn parses_mvs_quantization_control_update() {
    let mut update = Vec::with_capacity(129);
    update.push(2);
    update.extend(0_u8..64);
    update.extend(64_u8..128);
    let mut payload = (update.len() as u32).to_be_bytes().to_vec();
    payload.extend_from_slice(&update);

    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(1, 1).unwrap();
    assert_eq!(
        decoder
            .decode_rectangle(rect(1, 1, Encoding::ArdMvs), &payload, &mut framebuffer)
            .unwrap(),
        payload.len()
    );
    let (luminance, chrominance) = decoder.mvs_quantization_tables();
    assert_eq!(luminance[0], 0);
    assert_eq!(luminance[63], 63);
    assert_eq!(chrominance[0], 64);
    assert_eq!(chrominance[63], 127);
}

#[test]
fn parses_zero_sized_mvs_quantization_control_update() {
    let mut update = Vec::with_capacity(129);
    update.push(2);
    update.extend(0_u8..128);
    let mut payload = (update.len() as u32).to_be_bytes().to_vec();
    payload.extend_from_slice(&update);

    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(1, 1).unwrap();
    assert_eq!(
        decoder
            .decode_rectangle(rect(0, 0, Encoding::ArdMvs), &payload, &mut framebuffer)
            .unwrap(),
        payload.len()
    );
    let (luminance, chrominance) = decoder.mvs_quantization_tables();
    assert_eq!(luminance[0], 0);
    assert_eq!(luminance[63], 63);
    assert_eq!(chrominance[0], 64);
    assert_eq!(chrominance[63], 127);
}

#[test]
fn decodes_native_screen_sharing_oracle_mvs_packet() {
    // Exact 64x64 type-0 packet accepted and displayed as white by macOS
    // Screen Sharing 6.1 (760.4) on 2026-07-25.
    let packet = partial_mvs_packet(&[
        (0, 1),    // initial state
        (0, 3),    // white tile
        (1, 1),    // extended repeat
        (15, 4),   // repeat base: 16
        (47, 8),   // repeat extension: total repeat count 63
        (0x6d, 8), // primary marker
    ]);
    assert_eq!(packet.len(), 15);

    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(64, 64).unwrap();
    assert_eq!(
        decoder
            .decode_rectangle(rect(64, 64, Encoding::ArdMvs), &packet, &mut framebuffer)
            .unwrap(),
        packet.len()
    );
    assert!(
        framebuffer
            .rgba()
            .chunks_exact(4)
            .all(|pixel| pixel == [255, 255, 255, 255])
    );
}

#[test]
fn decodes_native_screen_sharing_solid_oracle_packet() {
    // Exact dual-stream structure accepted by Screen Sharing on 2026-07-25:
    // one neutral-gray type-4 tile followed by 63 white tiles.
    let packet = partial_mvs_packet_with_secondary(
        &[
            (0, 1),
            (4, 3),
            (0, 1),
            (0, 3),
            (1, 1),
            (15, 4),
            (46, 8),
            (0x6d, 8),
        ],
        &[(0, 1), (0, 1), (200, 8), (32, 6), (32, 6)],
    );

    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(64, 64).unwrap();
    decoder
        .decode_rectangle(rect(64, 64, Encoding::ArdMvs), &packet, &mut framebuffer)
        .unwrap();
    for y in 0..64 {
        for x in 0..64 {
            let offset = (y * 64 + x) * 4;
            let expected = if x < 8 && y < 8 {
                [200, 200, 200, 255]
            } else {
                [255, 255, 255, 255]
            };
            assert_eq!(&framebuffer.rgba()[offset..offset + 4], expected);
        }
    }
}

#[test]
fn decodes_native_type_four_bilevel_color_bits() {
    // DecodeMVSUpdate selects the second color for a zero bit and the first
    // color for a one bit. Each row below therefore starts black and ends
    // with seven white pixels.
    let mut secondary = vec![
        (1, 1),   // two-color tile
        (0, 1),   // replace both remembered colors
        (255, 8), // first color: neutral white
        (32, 6),
        (32, 6),
        (0, 8), // second color: neutral black
        (32, 6),
        (32, 6),
        (0, 8), // every row has an explicit bit mask
    ];
    secondary.extend(std::iter::repeat_n((0x7f, 8), 8));
    let packet =
        partial_mvs_packet_with_secondary(&[(0, 1), (4, 3), (0, 1), (0x6d, 8)], &secondary);

    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(8, 8).unwrap();
    decoder
        .decode_rectangle(rect(8, 8, Encoding::ArdMvs), &packet, &mut framebuffer)
        .unwrap();

    for y in 0..8 {
        for x in 0..8 {
            let offset = (y * 8 + x) * 4;
            let expected = if x == 0 {
                [0, 0, 0, 255]
            } else {
                [255, 255, 255, 255]
            };
            assert_eq!(&framebuffer.rgba()[offset..offset + 4], expected);
        }
    }
}

#[test]
fn decodes_native_screen_sharing_zero_dct_oracle_packet() {
    // Exact minimum Rice/DCT record accepted by Screen Sharing on 2026-07-25.
    // Zero DC predictors plus an immediate AC end-of-block produce gray.
    let packet = partial_mvs_packet_with_secondary(
        &[
            (0, 1),
            (5, 3),
            (0, 1),
            (0, 3),
            (1, 1),
            (15, 4),
            (46, 8),
            (0x6d, 8),
        ],
        &[
            (0, 1), // decode rather than reuse a prior block
            (3, 2), // retain both chroma DC predictors
            (0, 1), // zero DC prefix
            (0, 1), // zero DC magnitude
            (0, 1), // AC short-control form
            (1, 2), // end-of-block selector
            (0, 1), // end block
        ],
    );

    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(64, 64).unwrap();
    decoder
        .decode_rectangle(rect(64, 64, Encoding::ArdMvs), &packet, &mut framebuffer)
        .unwrap();
    for y in 0..64 {
        for x in 0..64 {
            let offset = (y * 64 + x) * 4;
            let expected = if x < 8 && y < 8 {
                [128, 128, 128, 255]
            } else {
                [255, 255, 255, 255]
            };
            assert_eq!(&framebuffer.rgba()[offset..offset + 4], expected);
        }
    }
}

#[test]
fn decodes_mvs_rice_ac_coefficient_and_block_reuse() {
    let packet = partial_mvs_packet_with_secondary(
        &[(0, 1), (5, 3), (0, 1), (5, 3), (0, 1), (0x6d, 8)],
        &[
            (0, 1), // new coefficient block
            (3, 2), // retain zero chroma predictors
            (0, 1),
            (0, 1), // zero DC
            (0, 1),
            (2, 2), // positive base coefficient at zigzag index 1
            (0, 1),
            (1, 2),
            (0, 1), // end block
            (1, 1), // reuse the preceding coefficient block
        ],
    );
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(16, 8).unwrap();
    decoder
        .decode_rectangle(rect(16, 8, Encoding::ArdMvs), &packet, &mut framebuffer)
        .unwrap();

    for y in 0..8 {
        assert_eq!(
            &framebuffer.rgba()[(y * 16) * 4..(y * 16 + 8) * 4],
            &framebuffer.rgba()[(y * 16 + 8) * 4..(y * 16 + 16) * 4]
        );
    }
    let first = framebuffer.rgba()[0];
    let last = framebuffer.rgba()[7 * 4];
    assert_eq!(
        framebuffer.rgba()[..8 * 4]
            .chunks_exact(4)
            .map(|pixel| pixel[0])
            .collect::<Vec<_>>(),
        [159, 154, 145, 134, 122, 111, 102, 97]
    );
    assert_eq!(first, 159);
    assert_eq!(last, 97);
    assert!(
        framebuffer
            .rgba()
            .chunks_exact(4)
            .all(|pixel| pixel[0] == pixel[1] && pixel[1] == pixel[2] && pixel[3] == 255)
    );
}

#[test]
fn decodes_mvs_full_differential_and_both_cache_selectors() {
    let mut baseline = partial_mvs_packet_with_secondary(
        &[(0, 1), (5, 3), (0, 1), (0x6d, 8)],
        &[
            (0, 1), // new coefficient block
            (3, 2), // retain zero chroma predictors
            (0, 1),
            (0, 1), // zero DC
            (0, 1),
            (2, 2), // positive AC coefficient
            (0, 1),
            (1, 2),
            (0, 1), // end block
        ],
    );
    baseline[5] = 2;
    baseline[6] = 2;
    let differential = full_mvs_packet(
        [64, 64],
        &[
            (1, 2),      // differential DCT selector
            (1, 6),      // two luma coefficients
            (1, 3),      // increase the signed AC coefficient by one
            (0, 1),      // unchanged Cr DC
            (0, 2),      // JPEG luminance AC symbol 0x01 (00)
            (1, 1),      // positive Cr AC coefficient
            (0b1010, 4), // JPEG luminance AC end-of-block
            (0, 1),      // unchanged Cb DC
            (0b1010, 4), // JPEG luminance AC end-of-block
        ],
    );
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(8, 8).unwrap();
    decoder
        .decode_rectangle(rect(8, 8, Encoding::ArdMvs), &baseline, &mut framebuffer)
        .unwrap();
    decoder
        .decode_rectangle(
            rect(8, 8, Encoding::ArdMvs),
            &differential,
            &mut framebuffer,
        )
        .unwrap();
    let expected_row = [149, 147, 141, 132, 124, 115, 109, 107];
    assert_eq!(
        framebuffer.rgba()[..32]
            .chunks_exact(4)
            .map(|pixel| pixel[0])
            .collect::<Vec<_>>(),
        expected_row
    );
    assert_eq!(
        framebuffer.rgba()[..32]
            .chunks_exact(4)
            .map(|pixel| pixel.try_into().unwrap())
            .collect::<Vec<[u8; 4]>>(),
        [
            [149, 143, 145, 255],
            [147, 141, 143, 255],
            [141, 137, 138, 255],
            [132, 130, 131, 255],
            [124, 126, 125, 255],
            [115, 119, 118, 255],
            [109, 115, 113, 255],
            [107, 113, 111, 255],
        ]
    );

    // Full refinements remain relative to the original partial-update
    // Rice-DCT baseline.
    decoder
        .decode_rectangle(
            rect(8, 8, Encoding::ArdMvs),
            &differential,
            &mut framebuffer,
        )
        .unwrap();
    assert_eq!(
        framebuffer.rgba()[..32]
            .chunks_exact(4)
            .map(|pixel| pixel[0])
            .collect::<Vec<_>>(),
        expected_row
    );

    let white = partial_mvs_packet(&[(0, 1), (0, 3), (0, 1), (0x6d, 8)]);
    decoder
        .decode_rectangle(rect(8, 8, Encoding::ArdMvs), &white, &mut framebuffer)
        .unwrap();
    let partial_cache =
        partial_mvs_packet_with_secondary(&[(0, 1), (6, 3), (0, 1), (0x6d, 8)], &[(0, 8), (1, 8)]);
    decoder
        .decode_rectangle(
            rect(8, 8, Encoding::ArdMvs),
            &partial_cache,
            &mut framebuffer,
        )
        .unwrap();
    assert_eq!(
        framebuffer.rgba()[..32]
            .chunks_exact(4)
            .map(|pixel| pixel[0])
            .collect::<Vec<_>>(),
        expected_row
    );

    decoder
        .decode_rectangle(rect(8, 8, Encoding::ArdMvs), &white, &mut framebuffer)
        .unwrap();
    let full_cache = full_mvs_packet(
        [0, 0],
        &[
            (3, 2), // DCT cache selector
            (0, 1), // explicit rather than sequential index
            (0, 8),
            (1, 8),
        ],
    );
    decoder
        .decode_rectangle(rect(8, 8, Encoding::ArdMvs), &full_cache, &mut framebuffer)
        .unwrap();
    assert_eq!(
        framebuffer.rgba()[..32]
            .chunks_exact(4)
            .map(|pixel| pixel[0])
            .collect::<Vec<_>>(),
        expected_row
    );
}

#[test]
fn full_differential_cache_recall_truncates_high_chroma_ac_like_native() {
    // Screen Sharing's DCT cache stores only the first 15 Cr and first 20 Cb
    // zigzag coefficients. A freshly rendered differential tile keeps all of
    // them, but a later cache recall renders the truncated block. The test
    // places a Cr coefficient at zigzag position 17 and checks that the
    // recalled tile matches a reference tile whose Cr AC never existed.
    let baseline = partial_mvs_packet_with_secondary(
        &[(0, 1), (5, 3), (0, 1), (0x6d, 8)],
        &[
            (0, 1), // new coefficient block
            (3, 2), // retain zero chroma predictors
            (0, 1),
            (0, 1), // zero DC
            (0, 1),
            (2, 2), // coefficient 16 at scan one under the zero limit
            (0, 1),
            (1, 2),
            (0, 1), // end block
        ],
    );

    let mut differential_with_high_cr = vec![
        (1, 2), // differential DCT selector
        (1, 6), // two luma coefficients
        (0, 4), // refine the nonzero baseline AC at scan one
        (0, 1),
        (0, 1), // append a zero Rice record at scan two
        (0, 1), // unchanged Cr DC
    ];
    differential_with_high_cr.extend(huffman_ac_symbol(0xf0)); // skip 16 scans
    differential_with_high_cr.extend(huffman_ac_symbol(0x01)); // +1 at scan 17
    differential_with_high_cr.push((1, 1)); // positive one-bit magnitude
    differential_with_high_cr.extend(huffman_ac_symbol(0x00)); // Cr end of block
    differential_with_high_cr.push((0, 1)); // unchanged Cb DC
    differential_with_high_cr.extend(huffman_ac_symbol(0x00)); // Cb end of block
    let differential_with_high_cr = full_mvs_packet([64, 64], &differential_with_high_cr);

    let mut differential_without_cr = vec![
        (1, 2),
        (1, 6),
        (0, 4),
        (0, 1),
        (0, 1),
        (0, 1), // unchanged Cr DC
    ];
    differential_without_cr.extend(huffman_ac_symbol(0x00)); // Cr end of block
    differential_without_cr.push((0, 1)); // unchanged Cb DC
    differential_without_cr.extend(huffman_ac_symbol(0x00)); // Cb end of block
    let differential_without_cr = full_mvs_packet([64, 64], &differential_without_cr);

    let recall = partial_mvs_packet_with_secondary(
        &[(0, 1), (6, 3), (0, 1), (0x6d, 8)],
        &[(0, 8), (1, 8)], // explicit cache index 1
    );

    let mut first = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(8, 8).unwrap();
    first
        .decode_rectangle(rect(8, 8, Encoding::ArdMvs), &baseline, &mut framebuffer)
        .unwrap();
    first
        .decode_rectangle(
            rect(8, 8, Encoding::ArdMvs),
            &differential_with_high_cr,
            &mut framebuffer,
        )
        .unwrap();
    let full_render = framebuffer.rgba();
    first
        .decode_rectangle(rect(8, 8, Encoding::ArdMvs), &recall, &mut framebuffer)
        .unwrap();
    let truncated_recall = framebuffer.rgba();
    assert_ne!(
        full_render, truncated_recall,
        "high Cr AC must be visible first"
    );

    let mut second = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut reference = Framebuffer::new(8, 8).unwrap();
    second
        .decode_rectangle(rect(8, 8, Encoding::ArdMvs), &baseline, &mut reference)
        .unwrap();
    second
        .decode_rectangle(
            rect(8, 8, Encoding::ArdMvs),
            &differential_without_cr,
            &mut reference,
        )
        .unwrap();
    second
        .decode_rectangle(rect(8, 8, Encoding::ArdMvs), &recall, &mut reference)
        .unwrap();
    assert_eq!(reference.rgba(), truncated_recall);
}

#[test]
fn consecutive_full_differentials_keep_the_partial_rice_baseline() {
    let mut baseline = partial_mvs_packet_with_secondary(
        &[(0, 1), (5, 3), (0, 1), (0x6d, 8)],
        &[
            (0, 1), // new coefficient block
            (3, 2), // retain zero chroma predictors
            (0, 1),
            (0, 1), // zero DC
            (0, 1),
            (2, 2), // positive AC coefficient
            (0, 1),
            (1, 2),
            (0, 1), // end block
        ],
    );
    baseline[5] = 2;
    baseline[6] = 2;
    let differential = full_mvs_packet(
        [64, 64],
        &[
            (1, 2), // differential DCT selector
            (2, 6), // three luma coefficients
            (1, 3), // refine the existing AC coefficient
            (1, 1),
            (0, 1),
            (0, 2),      // append Rice-coded coefficient +2
            (0, 1),      // unchanged Cr DC
            (0b1010, 4), // Cr end-of-block in the native luminance AC table
            (0, 1),      // unchanged Cb DC
            (0b1010, 4), // Cb end-of-block in the native luminance AC table
        ],
    );

    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(8, 8).unwrap();
    decoder
        .decode_rectangle(rect(8, 8, Encoding::ArdMvs), &baseline, &mut framebuffer)
        .unwrap();
    decoder
        .decode_rectangle(
            rect(8, 8, Encoding::ArdMvs),
            &differential,
            &mut framebuffer,
        )
        .unwrap();
    let first = framebuffer.rgba().to_vec();

    decoder
        .decode_rectangle(
            rect(8, 8, Encoding::ArdMvs),
            &differential,
            &mut framebuffer,
        )
        .unwrap();
    assert_eq!(framebuffer.rgba(), first);
}

#[test]
fn mvs_sequential_cache_recall_is_independent_of_insertion_cursor() {
    let mut baseline = partial_mvs_packet_with_secondary(
        &[(0, 1), (5, 3), (0, 1), (0x6d, 8)],
        &[
            (0, 1), // new coefficient block
            (3, 2), // retain zero chroma predictors
            (0, 1),
            (0, 1), // zero DC
            (0, 1),
            (2, 2), // positive AC coefficient
            (0, 1),
            (1, 2),
            (0, 1), // end block
        ],
    );
    baseline[5] = 2;
    baseline[6] = 2;
    let differential = full_mvs_packet(
        [64, 64],
        &[
            (1, 2),      // differential DCT selector; inserts cache entry 1
            (1, 6),      // two luma coefficients
            (1, 3),      // increase the signed AC coefficient by one
            (0, 1),      // unchanged Cr DC
            (0b1010, 4), // JPEG luminance AC end-of-block
            (0, 1),      // unchanged Cb DC
            (0b1010, 4), // JPEG luminance AC end-of-block
        ],
    );
    let sequential_cache =
        partial_mvs_packet_with_secondary(&[(0, 1), (7, 3), (0, 1), (0x6d, 8)], &[]);

    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(8, 8).unwrap();
    decoder
        .decode_rectangle(rect(8, 8, Encoding::ArdMvs), &baseline, &mut framebuffer)
        .unwrap();
    decoder
        .decode_rectangle(
            rect(8, 8, Encoding::ArdMvs),
            &differential,
            &mut framebuffer,
        )
        .unwrap();
    let expected = framebuffer.rgba().to_vec();
    let white = partial_mvs_packet(&[(0, 1), (0, 3), (0, 1), (0x6d, 8)]);
    decoder
        .decode_rectangle(rect(8, 8, Encoding::ArdMvs), &white, &mut framebuffer)
        .unwrap();
    assert_ne!(framebuffer.rgba(), expected);
    decoder
        .decode_rectangle(
            rect(8, 8, Encoding::ArdMvs),
            &sequential_cache,
            &mut framebuffer,
        )
        .unwrap();

    assert_eq!(framebuffer.rgba(), expected);

    // A later insertion advances only the write cursor. The next sequential
    // recall still advances from the preceding cache reference (1 -> 2).
    decoder
        .decode_rectangle(
            rect(8, 8, Encoding::ArdMvs),
            &differential,
            &mut framebuffer,
        )
        .unwrap();
    let expected = framebuffer.rgba().to_vec();
    decoder
        .decode_rectangle(rect(8, 8, Encoding::ArdMvs), &white, &mut framebuffer)
        .unwrap();
    let full_sequential_cache = full_mvs_packet(
        [0, 0],
        &[(3, 2), (1, 1)], // cache selector, sequential index
    );
    decoder
        .decode_rectangle(
            rect(8, 8, Encoding::ArdMvs),
            &full_sequential_cache,
            &mut framebuffer,
        )
        .unwrap();
    assert_eq!(framebuffer.rgba(), expected);
}

#[test]
fn missing_mvs_cache_recall_is_a_native_noop() {
    let mut baseline = partial_mvs_packet_with_secondary(
        &[(0, 1), (5, 3), (0, 1), (0x6d, 8)],
        &[
            (0, 1), // new coefficient block
            (3, 2), // retain zero chroma predictors
            (0, 1),
            (0, 1), // zero DC
            (0, 1),
            (2, 2), // positive AC coefficient
            (0, 1),
            (1, 2),
            (0, 1), // end block
        ],
    );
    baseline[5] = 2;
    baseline[6] = 2;
    let differential = full_mvs_packet(
        [64, 64],
        &[
            (1, 2),      // differential DCT selector; inserts cache entry 1
            (1, 6),      // two luma coefficients
            (1, 3),      // increase the signed AC coefficient by one
            (0, 1),      // unchanged Cr DC
            (0b1010, 4), // JPEG luminance AC end-of-block
            (0, 1),      // unchanged Cb DC
            (0b1010, 4), // JPEG luminance AC end-of-block
        ],
    );
    let explicit_cache =
        partial_mvs_packet_with_secondary(&[(0, 1), (6, 3), (0, 1), (0x6d, 8)], &[(0, 8), (1, 8)]);
    let full_sequential_cache = full_mvs_packet(
        [0, 0],
        &[(3, 2), (1, 1)], // asks for cache entry 2 after referencing entry 1
    );

    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(8, 8).unwrap();
    decoder
        .decode_rectangle(rect(8, 8, Encoding::ArdMvs), &baseline, &mut framebuffer)
        .unwrap();
    decoder
        .decode_rectangle(
            rect(8, 8, Encoding::ArdMvs),
            &differential,
            &mut framebuffer,
        )
        .unwrap();
    decoder
        .decode_rectangle(
            rect(8, 8, Encoding::ArdMvs),
            &explicit_cache,
            &mut framebuffer,
        )
        .unwrap();
    let before = framebuffer.rgba().to_vec();
    assert_eq!(
        decoder
            .decode_rectangle(
                rect(8, 8, Encoding::ArdMvs),
                &full_sequential_cache,
                &mut framebuffer,
            )
            .unwrap(),
        full_sequential_cache.len()
    );
    assert_eq!(framebuffer.rgba(), before);
}

#[test]
fn decodes_zero_limit_mvs_differential_baseline() {
    let baseline = partial_mvs_packet_with_secondary(
        &[(0, 1), (5, 3), (0, 1), (0x6d, 8)],
        &[
            (0, 1),
            (3, 2),
            (0, 1),
            (0, 1), // zero DC
            (0, 1),
            (2, 2), // coefficient 16 when the compact limit is zero
            (0, 1),
            (1, 2),
            (0, 1),
        ],
    );
    let differential = full_mvs_packet(
        [0, 0],
        &[
            (1, 2),
            (0, 6), // new compact length one
            (0, 4), // refine the baseline coefficient at scan one (discarded)
            (0, 1), // unchanged Cr DC
            (0, 1), // unchanged Cb DC
        ],
    );
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(8, 8).unwrap();
    decoder
        .decode_rectangle(rect(8, 8, Encoding::ArdMvs), &baseline, &mut framebuffer)
        .unwrap();
    decoder
        .decode_rectangle(
            rect(8, 8, Encoding::ArdMvs),
            &differential,
            &mut framebuffer,
        )
        .unwrap();
    assert!(
        framebuffer
            .rgba()
            .chunks_exact(4)
            .all(|pixel| pixel == [128, 128, 128, 255])
    );
}

#[test]
fn decodes_mvs_full_ac_at_limit_one() {
    let packet = full_mvs_packet(
        [1, 1],
        &[
            (1, 2), // differential DCT selector
            (0, 6), // one luma coefficient: the DC value only
            (0, 1),
            (0, 1),      // zero Rice record for the empty baseline at scan one
            (0, 1),      // unchanged Cr DC baseline
            (0b1010, 4), // native luminance AC end-of-block
            (0, 1),      // unchanged Cb DC baseline
            (0b1010, 4), // native luminance AC end-of-block
        ],
    );
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(8, 8).unwrap();
    assert_eq!(
        decoder
            .decode_rectangle(rect(8, 8, Encoding::ArdMvs), &packet, &mut framebuffer)
            .unwrap(),
        packet.len()
    );
}

#[test]
fn decodes_mvs_differential_from_native_zero_baseline() {
    let differential = full_mvs_packet(
        [0, 0],
        &[
            (1, 2), // differential DCT selector
            (0, 6), // one luma coefficient
            (0, 1),
            (0, 1), // zero Rice record for the empty baseline at scan one
            (0, 1), // unchanged Cr DC
            (0, 1), // unchanged Cb DC
        ],
    );
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(8, 8).unwrap();
    decoder
        .decode_rectangle(
            rect(8, 8, Encoding::ArdMvs),
            &differential,
            &mut framebuffer,
        )
        .unwrap();
    assert!(
        framebuffer
            .rgba()
            .chunks_exact(4)
            .all(|pixel| pixel == [128, 128, 128, 255])
    );
}

#[test]
fn replays_a_partial_copy_tile_in_a_full_mvs_update() {
    let initial = partial_mvs_packet_with_secondary(
        &[
            (0, 1),
            (4, 3), // solid first tile
            (0, 1),
            (1, 3), // copy first tile to its right
            (0, 1),
            (0x6d, 8),
        ],
        &[
            (0, 1),
            (0, 1),
            (80, 8),
            (32, 6),
            (32, 6), // neutral dark gray
        ],
    );
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(16, 8).unwrap();
    decoder
        .decode_rectangle(rect(16, 8, Encoding::ArdMvs), &initial, &mut framebuffer)
        .unwrap();

    for row in 0..8 {
        for column in 0..8 {
            let offset = (row * 16 + column) * 4;
            framebuffer.pixels_mut()[offset..offset + 4].copy_from_slice(&[180, 90, 20, 0]);
        }
    }
    let replay = full_mvs_packet(
        [0, 0],
        &[
            (0, 2), // source tile remains as modified
            (2, 2), // replay the remembered left-copy operation
        ],
    );
    decoder
        .decode_rectangle(rect(16, 8, Encoding::ArdMvs), &replay, &mut framebuffer)
        .unwrap();
    assert!(
        framebuffer
            .rgba()
            .chunks_exact(4)
            .all(|pixel| pixel == [20, 90, 180, 255])
    );
}

#[test]
fn full_differential_preserves_partial_copy_source() {
    let mut baseline = partial_mvs_packet_with_secondary(
        &[
            (0, 1),
            (5, 3), // establish a DCT baseline for the first tile
            (0, 1),
            (5, 3), // reuse it for the second tile
            (0, 1),
            (0x6d, 8),
        ],
        &[
            (0, 1), // new coefficient block
            (3, 2), // retain zero chroma predictors
            (0, 1),
            (0, 1), // zero DC
            (0, 1),
            (2, 2), // positive AC coefficient
            (0, 1),
            (1, 2),
            (0, 1), // end block
            (1, 1), // reuse the block for the second tile
        ],
    );
    baseline[5] = 2;
    baseline[6] = 2;
    let partial_copy = partial_mvs_packet(&[
        (0, 1),
        (0, 3), // first tile becomes white
        (0, 1),
        (1, 3), // second tile remembers a left-copy source
        (0, 1),
        (0x6d, 8),
    ]);
    let differential = full_mvs_packet(
        [64, 64],
        &[
            (0, 2),      // first tile is unchanged
            (1, 2),      // refine the second tile without replacing its copy source
            (1, 6),      // two luma coefficients
            (1, 3),      // increase the signed AC coefficient by one
            (0, 1),      // unchanged Cr DC
            (0b1010, 4), // Cr end-of-block in the native luminance AC table
            (0, 1),      // unchanged Cb DC
            (0b1010, 4), // Cb end-of-block in the native luminance AC table
        ],
    );
    let replay = full_mvs_packet(
        [0, 0],
        &[
            (0, 2), // source tile remains unchanged
            (2, 2), // replay the remembered left-copy operation
        ],
    );

    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(16, 8).unwrap();
    for packet in [&baseline, &partial_copy, &differential] {
        decoder
            .decode_rectangle(rect(16, 8, Encoding::ArdMvs), packet, &mut framebuffer)
            .unwrap();
    }
    decoder
        .decode_rectangle(rect(16, 8, Encoding::ArdMvs), &replay, &mut framebuffer)
        .unwrap();

    assert!(
        framebuffer
            .rgba()
            .chunks_exact(4)
            .all(|pixel| pixel == [255, 255, 255, 255])
    );
}

#[test]
fn mvs_ycbcr_rounding_matches_screen_sharing_tables() {
    // This negative-chroma vector distinguishes Apple's rounded lookup table
    // from a signed arithmetic right shift (red would otherwise be 20).
    let packet = partial_mvs_packet_with_secondary(
        &[(0, 1), (4, 3), (0, 1), (0x6d, 8)],
        &[(0, 1), (0, 1), (200, 8), (0, 6), (0, 6)],
    );
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(8, 8).unwrap();
    decoder
        .decode_rectangle(rect(8, 8, Encoding::ArdMvs), &packet, &mut framebuffer)
        .unwrap();
    assert!(
        framebuffer
            .rgba()
            .chunks_exact(4)
            .all(|pixel| pixel == [21, 255, 0, 255])
    );
}

#[test]
fn decodes_mvs_partial_repeat_and_left_copy_modes() {
    // Type 0 with repeat-count 1 paints both tiles white in one command.
    let repeated = partial_mvs_packet(&[(0, 1), (0, 3), (1, 1), (0, 4), (0x6d, 8)]);
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(16, 1).unwrap();
    decoder
        .decode_rectangle(rect(16, 1, Encoding::ArdMvs), &repeated, &mut framebuffer)
        .unwrap();
    assert!(
        framebuffer
            .rgba()
            .chunks_exact(4)
            .all(|pixel| pixel == [255, 255, 255, 255])
    );

    // Type 3 paints the first tile, then type 1 copies it to the tile on the
    // left. The row-uniform bitmap is 0xff.
    let copied = partial_mvs_packet_with_secondary(
        &[(0, 1), (3, 3), (0, 1), (1, 3), (0, 1), (0x6d, 8)],
        &[(0xff, 8)],
    );
    let mut framebuffer = Framebuffer::new(16, 1).unwrap();
    decoder
        .decode_rectangle(rect(16, 1, Encoding::ArdMvs), &copied, &mut framebuffer)
        .unwrap();
    assert!(
        framebuffer
            .rgba()
            .chunks_exact(4)
            .all(|pixel| pixel == [255, 255, 255, 255])
    );
}

#[test]
fn mvs_partial_copy_uses_global_source_at_rectangle_edge() {
    let first_tile = partial_mvs_packet(&[(0, 1), (0, 3), (0, 1), (0x6d, 8)]);
    let second_tile = partial_mvs_packet(&[(0, 1), (1, 3), (0, 1), (0x6d, 8)]);
    let below_tile = partial_mvs_packet(&[(0, 1), (2, 3), (0, 1), (0x6d, 8)]);
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(16, 8).unwrap();

    decoder
        .decode_rectangle(rect(8, 8, Encoding::ArdMvs), &first_tile, &mut framebuffer)
        .unwrap();
    decoder
        .decode_rectangle(
            Rectangle {
                x: 8,
                ..rect(8, 8, Encoding::ArdMvs)
            },
            &second_tile,
            &mut framebuffer,
        )
        .unwrap();

    assert!(
        framebuffer
            .rgba()
            .chunks_exact(4)
            .all(|pixel| pixel == [255, 255, 255, 255])
    );

    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(8, 16).unwrap();
    decoder
        .decode_rectangle(rect(8, 8, Encoding::ArdMvs), &first_tile, &mut framebuffer)
        .unwrap();
    decoder
        .decode_rectangle(
            Rectangle {
                y: 8,
                ..rect(8, 8, Encoding::ArdMvs)
            },
            &below_tile,
            &mut framebuffer,
        )
        .unwrap();
    assert!(
        framebuffer
            .rgba()
            .chunks_exact(4)
            .all(|pixel| pixel == [255, 255, 255, 255])
    );
}

#[test]
fn mvs_full_copy_without_source_is_a_native_noop() {
    let packet = full_mvs_packet([0, 0], &[(2, 2)]);
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(8, 8).unwrap();
    framebuffer.pixels_mut().fill(0x5a);

    assert_eq!(
        decoder
            .decode_rectangle(rect(8, 8, Encoding::ArdMvs), &packet, &mut framebuffer)
            .unwrap(),
        packet.len()
    );
    assert!(framebuffer.pixels().iter().all(|&byte| byte == 0x5a));

    let mut gpu_decoder = Decoder::new_gpu_mvs(PixelFormat::XRGB8888).unwrap();
    let mut gpu_framebuffer = Framebuffer::new(8, 8).unwrap();
    gpu_decoder
        .decode_rectangle(rect(8, 8, Encoding::ArdMvs), &packet, &mut gpu_framebuffer)
        .unwrap();
    assert!(gpu_decoder.take_gpu_mvs_frames().is_empty());
}

#[test]
fn mvs_full_copy_with_stale_source_is_a_native_noop() {
    let initial = partial_mvs_packet(&[(0, 1), (0, 3), (0, 1), (1, 3), (0, 1), (0x6d, 8)]);
    let source_refresh = partial_mvs_packet(&[(0, 1), (0, 3), (0, 1), (0x6d, 8)]);
    let replay = full_mvs_packet([0, 0], &[(0, 2), (2, 2)]);
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(16, 8).unwrap();

    decoder
        .decode_rectangle(rect(16, 8, Encoding::ArdMvs), &initial, &mut framebuffer)
        .unwrap();
    decoder
        .decode_rectangle(
            rect(8, 8, Encoding::ArdMvs),
            &source_refresh,
            &mut framebuffer,
        )
        .unwrap();
    decoder
        .decode_rectangle(rect(16, 8, Encoding::ArdMvs), &replay, &mut framebuffer)
        .unwrap();

    assert!(
        framebuffer
            .rgba()
            .chunks_exact(4)
            .all(|pixel| pixel == [255, 255, 255, 255])
    );
}

#[test]
fn invalid_mvs_dct_reuse_is_transactional() {
    let packet = partial_mvs_packet_with_secondary(
        &[
            (0, 1),    // initial state
            (0, 3),    // first tile: white
            (0, 1),    // no repeats
            (5, 3),    // second tile: Rice/DCT cache reuse
            (0, 1),    // no repeats
            (0x6d, 8), // primary marker
        ],
        &[(1, 1)],
    );
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(16, 1).unwrap();
    framebuffer.pixels_mut().fill(7);
    assert_eq!(
        decoder
            .decode_rectangle(rect(16, 1, Encoding::ArdMvs), &packet, &mut framebuffer)
            .unwrap_err(),
        Error::Invalid("ARD MVS Rice/DCT reuse has no previous block")
    );
    assert!(framebuffer.pixels().iter().all(|&byte| byte == 7));
}

#[test]
fn decodes_mvs_full_update_skip_tiles_and_markers() {
    let mut update = vec![1, 64, 64];
    update.extend_from_slice(&packed_bits(&[
        (0, 2),
        (0, 2),
        (0x6d, 8),
        (0x76, 8),
        (0x73, 8),
    ]));
    let mut packet = (update.len() as u32).to_be_bytes().to_vec();
    packet.extend_from_slice(&update);

    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(16, 8).unwrap();
    framebuffer.pixels_mut().fill(0x5a);
    assert_eq!(
        decoder
            .decode_rectangle(rect(16, 8, Encoding::ArdMvs), &packet, &mut framebuffer,)
            .unwrap(),
        packet.len()
    );
    assert!(framebuffer.pixels().iter().all(|&byte| byte == 0x5a));
}

#[test]
fn decodes_mvs_full_differential_from_zero_initialized_baseline() {
    let packet = full_mvs_packet(
        [64, 64],
        &[
            (1, 2), // differential DCT selector
            (0, 6), // one luma coefficient: the DC value only
            (0, 1),
            (0, 1),      // zero Rice record for the empty baseline at scan one
            (0, 1),      // unchanged Cr DC baseline
            (0b1010, 4), // Cr AC end-of-block in the native luminance AC table
            (0, 1),      // unchanged Cb DC baseline
            (0b1010, 4), // Cb AC end-of-block in the native luminance AC table
        ],
    );
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(8, 8).unwrap();
    assert_eq!(
        decoder
            .decode_rectangle(rect(8, 8, Encoding::ArdMvs), &packet, &mut framebuffer)
            .unwrap(),
        packet.len()
    );
}

#[test]
fn copy_rect_is_overlap_safe() {
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(3, 1).unwrap();
    framebuffer
        .pixels_mut()
        .copy_from_slice(&[0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3, 0]);
    let rectangle = Rectangle {
        x: 1,
        y: 0,
        width: 2,
        height: 1,
        encoding: Encoding::CopyRect as i32,
    };
    decoder
        .decode_rectangle(rectangle, &[0, 0, 0, 0], &mut framebuffer)
        .unwrap();
    let red: Vec<u8> = framebuffer.rgba().chunks_exact(4).map(|p| p[0]).collect();
    assert_eq!(red, [1, 1, 2]);
}

#[test]
fn desktop_size_resets_the_framebuffer() {
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(1, 1).unwrap();
    let consumed = decoder
        .decode_rectangle(
            Rectangle {
                x: 0,
                y: 0,
                width: 3,
                height: 2,
                encoding: Encoding::DesktopSize as i32,
            },
            &[],
            &mut framebuffer,
        )
        .unwrap();
    assert_eq!(consumed, 0);
    assert_eq!((framebuffer.width(), framebuffer.height()), (3, 2));
    assert_eq!(framebuffer.pixels(), &[0; 24]);
}

#[test]
fn decodes_zrle_compact_pixels_when_padding_is_low_byte() {
    let high_xrgb = PixelFormat {
        red_shift: 24,
        green_shift: 16,
        blue_shift: 8,
        ..PixelFormat::XRGB8888
    };
    // Numeric pixel 0xFF000000 is transmitted as its three used numeric bytes.
    let zrle = [0, 0, 0, 255];
    let mut encoder = Compress::new(Compression::default(), true);
    let packet = compressed_packet(&mut encoder, &zrle);
    let mut decoder = Decoder::new(high_xrgb).unwrap();
    let mut framebuffer = Framebuffer::new_native(1, 1, high_xrgb).unwrap();
    decoder
        .decode_rectangle(rect(1, 1, Encoding::Zrle), &packet, &mut framebuffer)
        .unwrap();
    assert_eq!(framebuffer.rgba(), &[255, 0, 0, 255]);
}

#[test]
fn preserves_native_raw_pixel_bytes_when_requested() {
    let rgb565 = PixelFormat {
        bits_per_pixel: 16,
        depth: 16,
        big_endian: true,
        true_color: true,
        red_max: 31,
        green_max: 63,
        blue_max: 31,
        red_shift: 11,
        green_shift: 5,
        blue_shift: 0,
    };
    let wire_pixels = [0xf8, 0x00, 0x07, 0xe0, 0x00, 0x1f];
    let mut decoder = Decoder::new(rgb565).unwrap();
    let mut framebuffer = Framebuffer::new_native(3, 1, rgb565).unwrap();
    decoder
        .decode_rectangle(rect(3, 1, Encoding::Raw), &wire_pixels, &mut framebuffer)
        .unwrap();

    assert_eq!(framebuffer.format(), FramebufferFormat::Native(rgb565));
    assert_eq!(framebuffer.pixels(), &wire_pixels);
}
