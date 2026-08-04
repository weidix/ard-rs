use ard_rs::{
    Decoder, Encoding, Error, Framebuffer, PixelFormat, ProtocolVersion, Rectangle,
    parse_ard_auth_challenge, parse_ard_auth_response, parse_ard_client_init,
    parse_ard_session_options, parse_framebuffer_update, parse_security_types,
};
use flate2::{Compress, Compression, FlushCompress};

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
fn parses_live_macos_ard_banner_and_security_offer() {
    // Captured from this Mac's /System Screen Sharing server on 2026-07-25.
    let capture = b"RFB 003.889\n\x05\x1e\x21\x24\x02\x23";
    assert_eq!(
        ProtocolVersion::parse(capture).unwrap(),
        ProtocolVersion::ARD_3_889
    );
    let (types, consumed) = parse_security_types(&capture[12..], 36).unwrap();
    assert_eq!(consumed, 6);
    assert_eq!(types.len(), 5);
    assert!(
        types
            .iter()
            .any(|kind| matches!(kind, ard_rs::SecurityType::Apple(30)))
    );
    assert!(
        types
            .iter()
            .any(|kind| matches!(kind, ard_rs::SecurityType::VncAuthentication))
    );
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
fn decodes_mvs_partial_white_tile_and_markers() {
    // Initial state bit, update type 0, no repeats, then the primary marker.
    let packet = partial_mvs_packet(&[(0, 1), (0, 3), (0, 1), (0x6d, 8)]);
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(3, 2).unwrap();
    assert_eq!(
        decoder
            .decode_rectangle(rect(3, 2, Encoding::ArdMvs), &packet, &mut framebuffer)
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
            (1, 2),      // differential DCT selector
            (0, 6),      // one luma coefficient: the DC value only
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
            framebuffer.rgba_mut()[offset..offset + 4].copy_from_slice(&[20, 90, 180, 255]);
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
fn decodes_mvs_partial_solid_ycbcr_tile() {
    // Type 4, no repeats, solid/new-color flags, neutral-chroma red-ish Y.
    let packet = partial_mvs_packet_with_secondary(
        &[(0, 1), (4, 3), (0, 1), (0x6d, 8)],
        &[(0, 1), (0, 1), (200, 8), (32, 6), (32, 6)],
    );
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(1, 1).unwrap();
    decoder
        .decode_rectangle(rect(1, 1, Encoding::ArdMvs), &packet, &mut framebuffer)
        .unwrap();
    assert_eq!(framebuffer.rgba(), &[200, 200, 200, 255]);
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
    framebuffer.rgba_mut().fill(0x5a);

    assert_eq!(
        decoder
            .decode_rectangle(rect(8, 8, Encoding::ArdMvs), &packet, &mut framebuffer)
            .unwrap(),
        packet.len()
    );
    assert!(framebuffer.rgba().iter().all(|&byte| byte == 0x5a));

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
    framebuffer.rgba_mut().fill(7);
    assert_eq!(
        decoder
            .decode_rectangle(rect(16, 1, Encoding::ArdMvs), &packet, &mut framebuffer)
            .unwrap_err(),
        Error::Invalid("ARD MVS Rice/DCT reuse has no previous block")
    );
    assert!(framebuffer.rgba().iter().all(|byte| *byte == 7));
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
    framebuffer.rgba_mut().fill(0x5a);
    assert_eq!(
        decoder
            .decode_rectangle(rect(16, 8, Encoding::ArdMvs), &packet, &mut framebuffer,)
            .unwrap(),
        packet.len()
    );
    assert!(framebuffer.rgba().iter().all(|&byte| byte == 0x5a));
}

#[test]
fn decodes_mvs_full_differential_from_zero_initialized_baseline() {
    let packet = full_mvs_packet(
        [64, 64],
        &[
            (1, 2),      // differential DCT selector
            (0, 6),      // one luma coefficient: the DC value only
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
        .rgba_mut()
        .copy_from_slice(&[1, 0, 0, 255, 2, 0, 0, 255, 3, 0, 0, 255]);
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
    assert_eq!(framebuffer.rgba(), &[0; 24]);
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
    let mut framebuffer = Framebuffer::new(1, 1).unwrap();
    decoder
        .decode_rectangle(rect(1, 1, Encoding::Zrle), &packet, &mut framebuffer)
        .unwrap();
    assert_eq!(framebuffer.rgba(), &[255, 0, 0, 255]);
}
