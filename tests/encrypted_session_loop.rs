use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

use ard_rs::{
    ArdEncryptionControl, ArdMessageDispatcher, ArdServerMessage, ArdVerifiedRecordStream,
    ArdViewerInformation, Decoder, EncryptedTransportOracle, Framebuffer, PixelFormat,
    ProtocolVersion, build_ard_encryption_activation, build_ard_set_encryption_level,
    build_ard_type30_client_exchange, build_framebuffer_update_request, build_set_encodings,
    parse_ard_auth_challenge, parse_framebuffer_update, parse_security_types, parse_server_init,
    unwrap_ard_session_material,
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
    message
}

fn read_exact_vector(stream: &mut TcpStream, len: usize) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0_u8; len];
    stream.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn read_encrypted_record(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut length = [0_u8; 2];
    stream.read_exact(&mut length)?;
    let cipher_len = usize::from(u16::from_be_bytes(length));
    let mut wire = length.to_vec();
    wire.extend_from_slice(&read_exact_vector(stream, cipher_len)?);
    Ok(wire)
}

#[test]
fn rust_client_completes_full_encrypted_session_against_oracle() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();

    let server = thread::spawn(move || {
        let (stream, peer) = listener.accept().unwrap();
        let oracle = EncryptedTransportOracle {
            allowed_peer: Some(peer.ip()),
            expect_security_selection: false,
            ..EncryptedTransportOracle::default()
        };
        oracle.run(stream, peer).unwrap()
    });

    let mut stream = TcpStream::connect(address).unwrap();
    stream.set_nodelay(true).unwrap();

    // Banner and security offer.
    let banner = read_exact_vector(&mut stream, 12).unwrap();
    assert_eq!(
        ProtocolVersion::parse(&banner).unwrap(),
        ProtocolVersion::ARD_3_889
    );
    stream.write_all(b"RFB 003.889\n").unwrap();
    let offer = read_exact_vector(&mut stream, 2).unwrap();
    let (types, _) = parse_security_types(&offer, 4).unwrap();
    assert!(matches!(types[0], ard_rs::SecurityType::Apple(30)));
    // Apple's Screen Sharing client does not send a security-type selection
    // byte when the server offers a single type; the oracle's server side
    // therefore does not read one either.

    // Type-30 challenge and exchange.
    let challenge = read_exact_vector(&mut stream, 260).unwrap();
    let (challenge, _) = parse_ard_auth_challenge(&challenge, 512).unwrap();
    let mut private_random = vec![0_u8; 255];
    private_random.push(3);
    let exchange = build_ard_type30_client_exchange(
        &challenge,
        b"viewer",
        b"oracle",
        &private_random,
        [0x5a; 128],
        512,
    )
    .unwrap();
    stream
        .write_all(&exchange.response().encrypted_credentials)
        .unwrap();
    stream
        .write_all(&exchange.response().client_public_key)
        .unwrap();
    stream.flush().unwrap();

    let (_, authentication_value) = exchange.into_parts();
    assert_eq!(read_exact_vector(&mut stream, 4).unwrap(), [0, 0, 0, 0]);

    // ClientInit and extended ServerInit.
    stream.write_all(&[0xc1]).unwrap();
    let header = read_exact_vector(&mut stream, 24).unwrap();
    let payload_len = u32::from_be_bytes(header[20..24].try_into().unwrap()) as usize;
    let mut init = header;
    init.extend_from_slice(&read_exact_vector(&mut stream, payload_len).unwrap());
    let (server_init, _) = parse_server_init(&init, 1024).unwrap();
    assert_eq!((server_init.width, server_init.height), (64, 64));
    let extension = server_init.extension.expect("extended ServerInit");
    assert!(extension.supports_command(0x12));

    // Setup messages: viewer information, encryption proposal, encodings,
    // framebuffer update request.
    stream.write_all(&viewer_information_vector()).unwrap();
    stream
        .write_all(&build_ard_set_encryption_level(1, &[1]).unwrap())
        .unwrap();
    stream
        .write_all(&build_set_encodings(&[1011, 6, 16]).unwrap())
        .unwrap();
    stream
        .write_all(&build_framebuffer_update_request(false, 0, 0, 64, 64))
        .unwrap();
    stream.flush().unwrap();

    // The server answers with the 1103 control rectangle.
    let mut update = read_exact_vector(&mut stream, 4).unwrap();
    assert_eq!(update[0], 0);
    update.extend_from_slice(&read_exact_vector(&mut stream, 12).unwrap());
    update.extend_from_slice(&read_exact_vector(&mut stream, 36).unwrap());
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(64, 64).unwrap();
    assert_eq!(
        parse_framebuffer_update(&update, &mut decoder, &mut framebuffer).unwrap(),
        update.len()
    );
    let control = decoder.take_ard_encryption_control().unwrap();
    assert_eq!(control.command, ArdEncryptionControl::ENABLE_COMMAND);
    let material = unwrap_ard_session_material(&control, authentication_value);

    // Activation, then encrypted records in both directions.
    stream
        .write_all(&build_ard_encryption_activation())
        .unwrap();
    let mut client_encoder = material.record_encoder(u16::MAX as usize).unwrap();
    let mut verified = ArdVerifiedRecordStream::new(
        material.record_decoder(u16::MAX as usize).unwrap(),
        u16::MAX as usize,
        16,
    )
    .unwrap();
    let mut dispatcher = ArdMessageDispatcher::new(u16::MAX as usize, 64 * 1024).unwrap();
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(64, 64).unwrap();

    stream
        .write_all(
            &client_encoder
                .encode_wire(&build_framebuffer_update_request(true, 0, 0, 64, 64))
                .unwrap(),
        )
        .unwrap();
    stream.flush().unwrap();

    let mut saw_white = false;
    for _ in 0..8 {
        let wire = read_encrypted_record(&mut stream).unwrap();
        for payload in verified.push(&wire).unwrap() {
            let messages = dispatcher
                .push(&payload, &mut decoder, &mut framebuffer)
                .unwrap();
            for message in messages {
                if let ArdServerMessage::FramebufferUpdate {
                    rectangle_count: 1, ..
                } = message
                    && framebuffer
                        .rgba()
                        .chunks_exact(4)
                        .all(|pixel| pixel == [255, 255, 255, 255])
                {
                    saw_white = true;
                }
            }
        }
        if saw_white {
            break;
        }
    }
    assert!(saw_white, "white MVS frame was not decoded");
    drop(stream);

    let report = server.join().unwrap();
    assert!(report.activation_received);
    assert!(report.viewer_information.is_some());
    let proposal = report.set_encryption_level.expect("0x12 proposal");
    assert_eq!(proposal.command, 1);
    assert_eq!(proposal.level, 1);
    assert_eq!(proposal.methods, [1]);
    assert!(report.server_to_client_records >= 2);
    assert!(report.client_to_server_records >= 1);
    assert!(report.client_message_types.contains(&3));
}
