use ard_rs::{
    ArdMessageDispatcher, ArdServerMessage, Decoder, Framebuffer, PixelFormat, ProtocolVersion,
    SecurityType, parse_security_types,
};
use sha1::{Digest, Sha1};

fn decode_hex_fixture(text: &str) -> Vec<u8> {
    text.split_ascii_whitespace()
        .map(|octet| {
            assert_eq!(octet.len(), 2, "fixture octets must contain two hex digits");
            u8::from_str_radix(octet, 16).expect("fixture must contain hexadecimal octets")
        })
        .collect()
}

#[test]
fn replays_live_macos_ard_handshake_offline() {
    let capture = decode_hex_fixture(include_str!("fixtures/macos-ard-handshake.hex"));
    assert_eq!(capture.len(), 18);
    assert_eq!(
        ProtocolVersion::parse(&capture).unwrap(),
        ProtocolVersion::ARD_3_889
    );

    let (types, consumed) = parse_security_types(&capture[12..], 36).unwrap();
    assert_eq!(12 + consumed, capture.len());
    assert_eq!(
        types,
        [
            SecurityType::Apple(30),
            SecurityType::Apple(33),
            SecurityType::Apple(36),
            SecurityType::VncAuthentication,
            SecurityType::Apple(35),
        ]
    );
}

#[test]
fn decodes_saved_native_oracle_mvs_frame_without_a_connection() {
    let capture = decode_hex_fixture(include_str!("fixtures/native-mvs-white-64x64.hex"));
    assert_eq!(capture.len(), 31);

    let mut dispatcher = ArdMessageDispatcher::new(1024, 1024).unwrap();
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(64, 64).unwrap();
    let mut messages = Vec::new();

    // Replay deliberately fragmented input to match arbitrary TCP boundaries.
    for fragment in capture.chunks(7) {
        messages.extend(
            dispatcher
                .push(fragment, &mut decoder, &mut framebuffer)
                .unwrap(),
        );
    }

    assert_eq!(dispatcher.buffered_bytes(), 0);
    assert_eq!(
        messages,
        [ArdServerMessage::FramebufferUpdate {
            rectangle_count: 1,
            bytes: capture.len(),
        }]
    );
    assert!(
        framebuffer
            .rgba()
            .chunks_exact(4)
            .all(|pixel| pixel == [255, 255, 255, 255])
    );
}

#[test]
fn decodes_real_macos_mvs_capture_without_a_connection() {
    let capture = decode_hex_fixture(include_str!("fixtures/real-macos-mvs-256x256.hex"));
    assert_eq!(capture.len(), 4_448);

    let mut dispatcher = ArdMessageDispatcher::new(8 * 1024 * 1024, 1024).unwrap();
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(1920, 1080).unwrap();
    let mut messages = Vec::new();
    for fragment in capture.chunks(257) {
        messages.extend(
            dispatcher
                .push(fragment, &mut decoder, &mut framebuffer)
                .unwrap(),
        );
    }

    assert_eq!(dispatcher.buffered_bytes(), 0);
    assert_eq!(
        messages,
        [
            ArdServerMessage::FramebufferUpdate {
                rectangle_count: 1,
                bytes: 149,
            },
            ArdServerMessage::FramebufferUpdate {
                rectangle_count: 1,
                bytes: 4_299,
            },
        ]
    );

    let mut ppm = b"P6\n256 256\n255\n".to_vec();
    for y in 0..256 {
        for x in 0..256 {
            let offset = (y * usize::from(framebuffer.width()) + x) * 4;
            ppm.extend_from_slice(&framebuffer.rgba()[offset..offset + 3]);
        }
    }
    let digest: [u8; 20] = Sha1::digest(&ppm).into();
    assert_eq!(
        digest,
        [
            0x0a, 0xf5, 0x20, 0x14, 0x9d, 0x7f, 0xc1, 0x09, 0xfe, 0x76, 0xba, 0x8c, 0x35, 0xaa,
            0xe8, 0x44, 0x41, 0xef, 0x0c, 0x19,
        ]
    );
}
