use ard_rs::{
    ArdSetEncryptionLevel, Error, build_ard_encryption_activation, build_ard_set_encryption_level,
    parse_ard_set_encryption_level,
};

#[test]
fn builds_native_rfb_set_encryption_level_vector() {
    // Exact bytes written by the installed client's `_RFBSetEncryptionLevel`
    // for level 1 with the single ComCryption method: command 1, level 1,
    // one method, method 1.
    let message = build_ard_set_encryption_level(1, &[1]).unwrap();
    assert_eq!(
        message,
        [
            0x12, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01
        ]
    );
}

#[test]
fn builds_native_encryption_activation_vector() {
    // Exact eight bytes sent by the client's `_HandleFramebufferUpdate` after
    // accepting the 1103 control rectangle; screensharingd's
    // `HandleSetEncryptionMessage` treats command 2 with level 1 as the
    // transition to decrypt everything received.
    assert_eq!(
        build_ard_encryption_activation(),
        [0x12, 0x00, 0x00, 0x02, 0x00, 0x01, 0x00, 0x00]
    );
}

#[test]
fn parses_set_encryption_level_with_native_semantics() {
    let message = build_ard_set_encryption_level(1, &[1, 0x1234_5678]).unwrap();
    let (parsed, consumed) = parse_ard_set_encryption_level(&message, 16).unwrap();
    assert_eq!(consumed, message.len());
    assert_eq!(parsed.command, ArdSetEncryptionLevel::COMMAND_SET_METHODS);
    assert_eq!(parsed.level, 1);
    assert_eq!(parsed.methods, [1, 0x1234_5678]);

    let (activation, consumed) =
        parse_ard_set_encryption_level(&build_ard_encryption_activation(), 16).unwrap();
    assert_eq!(consumed, 8);
    assert_eq!(activation, ArdSetEncryptionLevel::activation());
}

#[test]
fn rejects_invalid_or_unbounded_set_encryption_level() {
    assert_eq!(
        build_ard_set_encryption_level(2, &[1]).unwrap_err(),
        Error::Invalid("unsupported ARD encryption level")
    );
    let too_many = vec![1; 101];
    assert_eq!(
        build_ard_set_encryption_level(1, &too_many).unwrap_err(),
        Error::LimitExceeded("ARD encryption method count")
    );

    let message = build_ard_set_encryption_level(1, &[1, 2]).unwrap();
    assert_eq!(
        parse_ard_set_encryption_level(&message, 1).unwrap_err(),
        Error::LimitExceeded("ARD encryption method count")
    );

    let mut activation = build_ard_encryption_activation();
    activation[6..8].copy_from_slice(&1_u16.to_be_bytes());
    assert_eq!(
        parse_ard_set_encryption_level(&activation, 16).unwrap_err(),
        Error::Invalid("ARD encryption activation has a nonzero method count")
    );

    let mut unknown_command = build_ard_set_encryption_level(1, &[1]).unwrap();
    unknown_command[2..4].copy_from_slice(&3_u16.to_be_bytes());
    assert_eq!(
        parse_ard_set_encryption_level(&unknown_command, 16).unwrap_err(),
        Error::Invalid("unsupported ARD encryption command")
    );

    let mut bad_padding = build_ard_set_encryption_level(1, &[1]).unwrap();
    bad_padding[1] = 1;
    assert_eq!(
        parse_ard_set_encryption_level(&bad_padding, 16).unwrap_err(),
        Error::Invalid("invalid ARD set-encryption-level padding")
    );
}

#[test]
fn set_encryption_level_parsing_handles_every_prefix_truncation() {
    let message = build_ard_set_encryption_level(1, &[1]).unwrap();
    for length in 0..message.len() {
        assert!(
            matches!(
                parse_ard_set_encryption_level(&message[..length], 16),
                Err(Error::NeedMore { .. })
            ),
            "length {length}"
        );
    }
}
