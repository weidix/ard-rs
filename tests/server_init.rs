use ard_rs::{Error, PixelFormat, build_ard_server_init, parse_server_init};

#[test]
fn extended_server_init_round_trips_with_command_support() {
    let mut support = [0_u8; 16];
    support[0] = 0xbe;
    support[2] = 0x20; // command 0x12, MSB-first
    let init =
        build_ard_server_init(64, 64, PixelFormat::XRGB8888, b"ard-rs oracle", 8, support).unwrap();
    let (parsed, consumed) = parse_server_init(&init, 1024).unwrap();
    assert_eq!(consumed, init.len());
    assert_eq!((parsed.width, parsed.height), (64, 64));
    assert_eq!(parsed.name, "ard-rs oracle");
    let extension = parsed.extension.expect("extended ServerInit");
    assert_eq!(extension.flags, 8);
    assert_eq!(extension.command_support, support);
    assert!(extension.supports_command(0x12));
    assert!(extension.supports_command(0));
    assert!(!extension.supports_command(1));
    assert!(!extension.supports_command(7));
    assert!(!extension.supports_command(0x11));
}

#[test]
fn standard_server_init_has_no_extension() {
    let mut init = Vec::new();
    init.extend_from_slice(&64_u16.to_be_bytes());
    init.extend_from_slice(&64_u16.to_be_bytes());
    init.extend_from_slice(&PixelFormat::XRGB8888.encode().unwrap());
    init.extend_from_slice(&4_u32.to_be_bytes());
    init.extend_from_slice(b"name");
    let (parsed, consumed) = parse_server_init(&init, 1024).unwrap();
    assert_eq!(consumed, init.len());
    assert_eq!(parsed.name, "name");
    assert!(parsed.extension.is_none());
}

#[test]
fn extended_server_init_handles_every_prefix_truncation() {
    let init =
        build_ard_server_init(64, 64, PixelFormat::XRGB8888, b"ard-rs oracle", 8, [0; 16]).unwrap();
    for length in 0..init.len() {
        assert!(
            matches!(
                parse_server_init(&init[..length], 1024),
                Err(Error::NeedMore { .. })
            ),
            "length {length}"
        );
    }
}

#[test]
fn server_init_rejects_non_utf8_names_and_oversized_payloads() {
    let mut init = Vec::new();
    init.extend_from_slice(&1_u16.to_be_bytes());
    init.extend_from_slice(&1_u16.to_be_bytes());
    init.extend_from_slice(&PixelFormat::XRGB8888.encode().unwrap());
    init.extend_from_slice(&2_u32.to_be_bytes());
    init.extend_from_slice(&[0xff, 0xfe]);
    assert_eq!(
        parse_server_init(&init, 1024).unwrap_err(),
        Error::Invalid("server name is not UTF-8")
    );

    let mut oversized = Vec::new();
    oversized.extend_from_slice(&1_u16.to_be_bytes());
    oversized.extend_from_slice(&1_u16.to_be_bytes());
    oversized.extend_from_slice(&PixelFormat::XRGB8888.encode().unwrap());
    oversized.extend_from_slice(&3_u32.to_be_bytes());
    oversized.extend_from_slice(&[b'x'; 3]);
    assert_eq!(
        parse_server_init(&oversized, 2).unwrap_err(),
        Error::LimitExceeded("server name length")
    );
}
