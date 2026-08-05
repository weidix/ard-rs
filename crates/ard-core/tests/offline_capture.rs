use ard_rs::{
    ArdMessageDispatcher, ArdServerMessage, Decoder, Framebuffer, PixelFormat, ProtocolVersion,
    SecurityType, parse_security_types,
};

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
            .pixels()
            .chunks_exact(4)
            .all(|pixel| pixel == [255, 255, 255, 0])
    );
}
