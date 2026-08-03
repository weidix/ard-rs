use ard_rs::{
    ArdMessageDispatcher, ArdServerMessage, Decoder, Framebuffer, PixelFormat, ProtocolVersion,
    SecurityType, parse_security_types,
};
use sha1::{Digest, Sha1};

#[test]
fn replays_live_macos_ard_handshake_offline() {
    let capture = include_bytes!("fixtures/macos-ard-handshake.bin");
    assert_eq!(capture.len(), 18);
    assert_eq!(
        ProtocolVersion::parse(capture).unwrap(),
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
    let capture = include_bytes!("fixtures/native-mvs-white-64x64.bin");
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
fn decodes_complete_real_macos_mvs_frame_without_a_connection() {
    let capture = include_bytes!("fixtures/real-macos-mvs-1920x1080.bin");
    assert_eq!(capture.len(), 53_215);

    let mut dispatcher = ArdMessageDispatcher::new(64 * 1024 * 1024, 1024 * 1024).unwrap();
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
                bytes: 53_066,
            },
        ]
    );
    assert!(
        framebuffer
            .rgba()
            .chunks_exact(4)
            .all(|pixel| pixel[3] == 255)
    );

    let mut ppm = b"P6\n1920 1080\n255\n".to_vec();
    for pixel in framebuffer.rgba().chunks_exact(4) {
        ppm.extend_from_slice(&pixel[..3]);
    }
    let digest: [u8; 20] = Sha1::digest(&ppm).into();
    assert_eq!(
        digest,
        [
            0x85, 0x8c, 0x9d, 0x68, 0xb7, 0x5a, 0x06, 0x4d, 0x3c, 0x73, 0x05, 0xe2, 0x53, 0xd3,
            0xa3, 0xfd, 0x81, 0xe4, 0x21, 0x50,
        ]
    );
}
