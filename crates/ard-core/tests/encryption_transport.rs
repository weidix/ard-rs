use ard_rs::{
    ArdEncryptedRecordFramer, ArdEncryptionControl, ArdSessionRecordDecoder,
    ArdSessionRecordEncoder, ArdVerifiedRecordStream, ArdViewerInformation, Decoder, Encoding,
    Error, Framebuffer, PixelFormat, parse_ard_encryption_control, parse_ard_viewer_information,
    parse_framebuffer_update, unwrap_ard_session_material,
};

fn viewer_information_vector() -> [u8; ArdViewerInformation::WIRE_LEN] {
    let mut message = [0; ArdViewerInformation::WIRE_LEN];
    message[0] = ArdViewerInformation::MESSAGE_TYPE;
    message[2..4].copy_from_slice(&(ArdViewerInformation::PAYLOAD_LEN as u16).to_be_bytes());
    message[4..6].copy_from_slice(&ArdViewerInformation::VERSION.to_be_bytes());
    for (index, component) in [2_u32, 6, 1, 0].into_iter().enumerate() {
        let offset = 6 + index * 4;
        message[offset..offset + 4].copy_from_slice(&component.to_be_bytes());
    }
    for (index, component) in [26_u32, 5, 2].into_iter().enumerate() {
        let offset = 22 + index * 4;
        message[offset..offset + 4].copy_from_slice(&component.to_be_bytes());
    }
    message[34] = 0xb0;
    message[36] = 0x0c;
    message[37] = 0x03;
    message[38] = 0x90;
    message[44] = 0x40;
    message
}

#[test]
fn parses_live_rfb_viewer_information_structure() {
    let message = viewer_information_vector();
    let (information, consumed) =
        parse_ard_viewer_information(&message, ArdViewerInformation::WIRE_LEN).unwrap();
    assert_eq!(consumed, message.len());
    assert_eq!(information.version, 1);
    assert_eq!(information.viewer_components, [2, 6, 1, 0]);
    assert_eq!(information.system_version, [26, 5, 2]);
    assert_eq!(information.capabilities[0], 0xb0);
    assert_eq!(information.capabilities[2..5], [0x0c, 0x03, 0x90]);
    assert_eq!(information.capabilities[10], 0x40);
}

#[test]
fn rejects_truncated_rfb_viewer_information() {
    let message = viewer_information_vector();
    for length in 0..message.len() {
        assert!(
            matches!(
                parse_ard_viewer_information(&message[..length], message.len()),
                Err(Error::NeedMore { .. })
            ),
            "length {length}"
        );
    }
}

#[test]
fn bounds_and_validates_rfb_viewer_information() {
    let message = viewer_information_vector();
    assert_eq!(
        parse_ard_viewer_information(&message, message.len() - 1).unwrap_err(),
        Error::LimitExceeded("RFBViewerInformation message")
    );

    let mut overlong = message.to_vec();
    overlong[2..4].copy_from_slice(&63_u16.to_be_bytes());
    overlong.push(0);
    assert_eq!(
        parse_ard_viewer_information(&overlong, overlong.len()).unwrap_err(),
        Error::Invalid("unsupported RFBViewerInformation length")
    );

    let mut invalid_padding = message;
    invalid_padding[1] = 1;
    assert_eq!(
        parse_ard_viewer_information(&invalid_padding, invalid_padding.len()).unwrap_err(),
        Error::Invalid("invalid RFBViewerInformation padding")
    );

    let mut invalid_version = viewer_information_vector();
    invalid_version[4..6].copy_from_slice(&2_u16.to_be_bytes());
    assert_eq!(
        parse_ard_viewer_information(&invalid_version, invalid_version.len()).unwrap_err(),
        Error::Invalid("unsupported RFBViewerInformation version")
    );
}

fn encryption_control_vector() -> [u8; ArdEncryptionControl::WIRE_LEN] {
    let mut payload = [0; ArdEncryptionControl::WIRE_LEN];
    payload[..4].copy_from_slice(&ArdEncryptionControl::ENABLE_COMMAND.to_be_bytes());
    for (index, byte) in payload[4..].iter_mut().enumerate() {
        *byte = index as u8;
    }
    payload
}

#[test]
fn parses_and_redacts_ard_encryption_control() {
    let payload = encryption_control_vector();
    let (control, consumed) = parse_ard_encryption_control(&payload).unwrap();
    assert_eq!(consumed, payload.len());
    assert_eq!(control.command, 1);
    assert_eq!(control.wrapped_session_blocks()[0], payload[4..20]);
    assert_eq!(control.wrapped_session_blocks()[1], payload[20..36]);

    let debug = format!("{control:?}");
    assert!(debug.contains("<redacted>"));
    assert!(!debug.contains("16, 17, 18"));

    assert!(matches!(
        parse_ard_encryption_control(&payload[..35]),
        Err(Error::NeedMore { .. })
    ));
    let mut invalid = payload;
    invalid[..4].copy_from_slice(&2_u32.to_be_bytes());
    assert_eq!(
        parse_ard_encryption_control(&invalid).unwrap_err(),
        Error::Invalid("unsupported ARD encryption command")
    );
}

#[test]
fn framebuffer_update_exposes_zero_sized_encryption_control() {
    let payload = encryption_control_vector();
    let mut update = vec![0, 0, 0, 1];
    update.extend_from_slice(&[0; 8]);
    update.extend_from_slice(&(Encoding::ArdEncryption as i32).to_be_bytes());
    update.extend_from_slice(&payload);

    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(1920, 1080).unwrap();
    assert_eq!(
        parse_framebuffer_update(&update, &mut decoder, &mut framebuffer).unwrap(),
        update.len()
    );
    let control = decoder.take_ard_encryption_control().unwrap();
    assert_eq!(control.command, ArdEncryptionControl::ENABLE_COMMAND);
    assert!(decoder.take_ard_encryption_control().is_none());
}

fn wire_record(ciphertext: &[u8]) -> Vec<u8> {
    let mut wire = (ciphertext.len() as u16).to_be_bytes().to_vec();
    wire.extend_from_slice(ciphertext);
    wire
}

#[test]
fn frames_records_across_every_tcp_fragment_boundary() {
    let ciphertext = [0x5a; 32];
    let wire = wire_record(&ciphertext);
    let mut framer = ArdEncryptedRecordFramer::new(1024, 8).unwrap();
    let mut records = Vec::new();
    for byte in wire {
        records.extend(framer.push(&[byte]).unwrap());
    }
    assert_eq!(records, [ciphertext.to_vec()]);
    assert_eq!(framer.buffered_bytes(), 0);
    assert_eq!(framer.expected_ciphertext_len(), None);
}

#[test]
fn frames_multiple_records_from_one_input() {
    let first = [0x11; 16];
    let second = [0x22; 48];
    let mut wire = wire_record(&first);
    wire.extend_from_slice(&wire_record(&second));
    let mut framer = ArdEncryptedRecordFramer::new(1024, 2).unwrap();
    assert_eq!(
        framer.push(&wire).unwrap(),
        [first.to_vec(), second.to_vec()]
    );
}

#[test]
fn record_framing_limits_and_errors_are_transactional() {
    let mut framer = ArdEncryptedRecordFramer::new(32, 1).unwrap();
    assert_eq!(framer.push(&[0]).unwrap(), Vec::<Vec<u8>>::new());
    assert_eq!(framer.buffered_bytes(), 1);

    assert_eq!(
        framer.push(&[31]).unwrap_err(),
        Error::Invalid("encrypted-record length is not an AES block multiple")
    );
    assert_eq!(framer.buffered_bytes(), 1);
    assert_eq!(framer.expected_ciphertext_len(), None);

    let mut rest = vec![32];
    rest.extend_from_slice(&[0x33; 32]);
    assert_eq!(framer.push(&rest).unwrap(), [vec![0x33; 32]]);

    assert_eq!(
        framer.push(&64_u16.to_be_bytes()).unwrap_err(),
        Error::LimitExceeded("encrypted record")
    );
    assert_eq!(framer.buffered_bytes(), 0);

    let mut two_records = wire_record(&[0x44; 16]);
    two_records.extend_from_slice(&wire_record(&[0x55; 16]));
    assert_eq!(
        framer.push(&two_records).unwrap_err(),
        Error::LimitExceeded("encrypted records per input")
    );
    assert_eq!(framer.buffered_bytes(), 0);
}

fn ciphertext_from_wire(wire: &[u8]) -> &[u8] {
    let declared = usize::from(u16::from_be_bytes([wire[0], wire[1]]));
    assert_eq!(declared, wire.len() - 2);
    &wire[2..]
}

#[test]
fn session_records_round_trip_and_advance_sequence() {
    let session_value = [0x6a; 16];
    let mut encoder = ArdSessionRecordEncoder::new(session_value, 1024).unwrap();
    let mut decoder = ArdSessionRecordDecoder::new(session_value, 1024).unwrap();

    let first = encoder.encode_wire(b"first message").unwrap();
    let second = encoder.encode_wire(b"second message").unwrap();
    assert_eq!(encoder.sequence(), 2);
    assert_eq!(
        decoder.decode(ciphertext_from_wire(&first)).unwrap(),
        b"first message"
    );
    assert_eq!(
        decoder.decode(ciphertext_from_wire(&second)).unwrap(),
        b"second message"
    );
    assert_eq!(decoder.sequence(), 2);
}

#[test]
fn session_record_tampering_and_replay_do_not_release_plaintext() {
    let session_value = [0x91; 16];
    let mut encoder = ArdSessionRecordEncoder::new(session_value, 1024).unwrap();
    let mut decoder = ArdSessionRecordDecoder::new(session_value, 1024).unwrap();
    let first = encoder.encode_wire(b"one").unwrap();
    let second = encoder.encode_wire(b"two").unwrap();

    assert_eq!(
        decoder.decode(ciphertext_from_wire(&first)).unwrap(),
        b"one"
    );
    let mut damaged = ciphertext_from_wire(&second).to_vec();
    let last = damaged.last_mut().unwrap();
    *last ^= 0x80;
    assert_eq!(
        decoder.decode(&damaged).unwrap_err(),
        Error::Invalid("encrypted-record checksum mismatch")
    );
    assert_eq!(decoder.sequence(), 1);
    assert_eq!(
        decoder.decode(ciphertext_from_wire(&second)).unwrap(),
        b"two"
    );
    assert_eq!(decoder.sequence(), 2);

    assert_eq!(
        decoder.decode(ciphertext_from_wire(&first)).unwrap_err(),
        Error::Invalid("encrypted-record checksum mismatch")
    );
    assert_eq!(decoder.sequence(), 2);
}

#[test]
fn session_plaintext_limits_are_transactional() {
    let session_value = [0xc3; 16];
    let mut oversized_encoder = ArdSessionRecordEncoder::new(session_value, 64).unwrap();
    let oversized = oversized_encoder.encode_wire(&[7; 5]).unwrap();
    let mut decoder = ArdSessionRecordDecoder::new(session_value, 4).unwrap();
    assert_eq!(
        decoder
            .decode(ciphertext_from_wire(&oversized))
            .unwrap_err(),
        Error::LimitExceeded("encrypted-record plaintext")
    );
    assert_eq!(decoder.sequence(), 0);

    let mut valid_encoder = ArdSessionRecordEncoder::new(session_value, 4).unwrap();
    let valid = valid_encoder.encode_wire(&[8; 4]).unwrap();
    assert_eq!(
        decoder.decode(ciphertext_from_wire(&valid)).unwrap(),
        [8; 4]
    );
    assert_eq!(decoder.sequence(), 1);
    assert_eq!(
        valid_encoder.encode_wire(&[0; 5]).unwrap_err(),
        Error::LimitExceeded("encrypted-record plaintext")
    );
}

#[test]
fn combined_stream_rolls_back_the_complete_input_batch() {
    let session_value = [0x47; 16];
    let mut encoder = ArdSessionRecordEncoder::new(session_value, 1024).unwrap();
    let first = encoder.encode_wire(b"alpha").unwrap();
    let second = encoder.encode_wire(b"beta").unwrap();
    let mut valid_batch = first;
    valid_batch.extend_from_slice(&second);

    let mut damaged_batch = valid_batch.clone();
    *damaged_batch.last_mut().unwrap() ^= 1;
    let decoder = ArdSessionRecordDecoder::new(session_value, 1024).unwrap();
    let mut stream = ArdVerifiedRecordStream::new(decoder, 1024, 8).unwrap();
    assert_eq!(
        stream.push(&damaged_batch).unwrap_err(),
        Error::Invalid("encrypted-record checksum mismatch")
    );
    assert_eq!(stream.sequence(), 0);
    assert_eq!(stream.buffered_bytes(), 0);

    assert_eq!(
        stream.push(&valid_batch).unwrap(),
        [b"alpha".to_vec(), b"beta".to_vec()]
    );
    assert_eq!(stream.sequence(), 2);
}

#[test]
fn control_blocks_initialize_session_value_and_cbc_chain() {
    let authentication_value = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f,
    ];
    // AES-128 example block: plaintext 001122...eeff becomes 69c4...c55a.
    let wrapped = [
        0x69, 0xc4, 0xe0, 0xd8, 0x6a, 0x7b, 0x04, 0x30, 0xd8, 0xcd, 0xb7, 0x80, 0x70, 0xb4, 0xc5,
        0x5a,
    ];
    let mut payload = [0; ArdEncryptionControl::WIRE_LEN];
    payload[..4].copy_from_slice(&1_u32.to_be_bytes());
    payload[4..20].copy_from_slice(&wrapped);
    payload[20..36].copy_from_slice(&wrapped);
    let (control, _) = parse_ard_encryption_control(&payload).unwrap();
    let material = unwrap_ard_session_material(&control, authentication_value);
    assert!(format!("{material:?}").contains("<redacted>"));

    let mut encoder = material.record_encoder(1024).unwrap();
    let mut decoder = material.record_decoder(1024).unwrap();
    let wire = encoder.encode_wire(b"uses nonzero initial chain").unwrap();
    assert_eq!(
        decoder.decode(ciphertext_from_wire(&wire)).unwrap(),
        b"uses nonzero initial chain"
    );
}
