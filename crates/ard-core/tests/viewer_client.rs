use std::net::TcpListener;
use std::thread;

use ard_rs::{
    ArdClient, ArdClientConfig, ArdClientEvent, ArdDisplayConfiguration, ArdFrameOutput,
    ArdReconnectPolicy, ArdVideoQuality, EncryptedTransportOracle, MvsGpuTile, PixelFormat,
};

#[test]
fn client_sends_fixed_display_configuration_inside_encrypted_transport() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (stream, peer) = listener.accept().unwrap();
        let mut command_support = EncryptedTransportOracle::default().command_support;
        command_support[3] |= 0x04;
        EncryptedTransportOracle {
            allowed_peer: Some(peer.ip()),
            expect_security_selection: false,
            command_support,
            ..EncryptedTransportOracle::default()
        }
        .run(stream, peer)
        .unwrap()
    });

    let mut config =
        ArdClientConfig::new(address.to_string(), b"viewer".to_vec(), b"oracle".to_vec());
    config.display_configuration = Some(ArdDisplayConfiguration::single(1920, 1080));
    let mut client = ArdClient::connect(config).unwrap();
    client.next_frame().unwrap();
    drop(client);

    let report = server.join().unwrap();
    assert_eq!(report.client_message_types[..2], [0x1d, 3]);
}

#[test]
fn receive_only_client_delivers_gpu_mvs_tiles_without_cpu_frame_expansion() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (stream, peer) = listener.accept().unwrap();
        EncryptedTransportOracle {
            allowed_peer: Some(peer.ip()),
            expect_security_selection: false,
            ..EncryptedTransportOracle::default()
        }
        .run(stream, peer)
        .unwrap()
    });

    let mut client = ArdClient::connect(ArdClientConfig::new(
        address.to_string(),
        b"viewer".to_vec(),
        b"oracle".to_vec(),
    ))
    .unwrap();
    let frame = client.next_frame().unwrap();
    assert_eq!(frame.index, 1);
    assert_eq!(frame.framebuffer_updates, 1);
    assert_eq!(frame.rectangle_count, 1);
    assert!(frame.wire_bytes > frame.payload_bytes);
    assert_eq!(
        (client.framebuffer().width(), client.framebuffer().height()),
        (64, 64)
    );
    assert!(client.framebuffer().pixels().is_empty());
    let gpu_frames = client.take_gpu_mvs_frames();
    assert_eq!(gpu_frames.len(), 1);
    assert_eq!(gpu_frames[0].tiles.len(), 64);
    assert!(
        gpu_frames[0]
            .tiles
            .iter()
            .all(|tile| matches!(tile.tile, MvsGpuTile::SolidRgba([255, 255, 255, 255])))
    );
    let frame = client.next_frame().unwrap();
    assert_eq!(frame.index, 2);
    assert_eq!(frame.framebuffer_updates, 1);
    assert_eq!(client.take_gpu_mvs_frames().len(), 1);

    drop(client);
    let report = server.join().unwrap();
    assert!(report.activation_received);
    assert_eq!(report.viewer_encodings, [1011, 1002, 6, 16, -223]);
    assert_eq!(report.client_message_types, [3, 9]);
    assert_eq!(report.client_framebuffer_update_incremental, [false]);
    let viewer = report
        .viewer_information
        .expect("viewer information received");
    assert_eq!(viewer.viewer_components, [2, 6, 1, 0]);
    assert_eq!(viewer.system_version, [26, 5, 2]);
    assert_eq!(viewer.capabilities[0], 0xb0);
    assert_eq!(viewer.capabilities[2..5], [0x0c, 0x03, 0x90]);
    assert_eq!(viewer.capabilities[10], 0x40);
}

#[test]
fn full_quality_client_negotiates_lossless_zlib_and_updates_native_pixels() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (stream, peer) = listener.accept().unwrap();
        EncryptedTransportOracle {
            allowed_peer: Some(peer.ip()),
            expect_security_selection: false,
            ..EncryptedTransportOracle::default()
        }
        .run(stream, peer)
        .unwrap()
    });

    let mut config =
        ArdClientConfig::new(address.to_string(), b"viewer".to_vec(), b"oracle".to_vec());
    config.video_quality = ArdVideoQuality::Full;
    let mut client = ArdClient::connect(config).unwrap();

    let first = client.next_frame().unwrap();
    assert_eq!(first.index, 1);
    assert_eq!(first.framebuffer_updates, 1);
    assert_eq!(client.framebuffer().pixels().len(), 64 * 64 * 4);
    assert_eq!(&client.framebuffer().pixels()[..4], &[255, 255, 255, 0]);
    assert!(client.take_gpu_mvs_frames().is_empty());

    let second = client.next_frame().unwrap();
    assert_eq!(second.index, 2);
    assert_eq!(&client.framebuffer().pixels()[..4], &[192, 96, 32, 0]);

    drop(client);
    let report = server.join().unwrap();
    assert_eq!(report.viewer_encodings, [6, 16, -223]);
    assert_eq!(report.client_message_types, [3, 9]);
    assert_eq!(report.client_framebuffer_update_incremental, [false]);
}

#[test]
fn client_can_retain_server_native_pixel_bytes() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (stream, peer) = listener.accept().unwrap();
        EncryptedTransportOracle {
            allowed_peer: Some(peer.ip()),
            expect_security_selection: false,
            ..EncryptedTransportOracle::default()
        }
        .run(stream, peer)
        .unwrap()
    });

    let mut config =
        ArdClientConfig::new(address.to_string(), b"viewer".to_vec(), b"oracle".to_vec());
    config.video_quality = ArdVideoQuality::Full;
    config.output_format = ArdFrameOutput::ServerNative;
    let mut client = ArdClient::connect(config).unwrap();
    client.next_frame().unwrap();

    assert_eq!(
        client.framebuffer().native_pixel_format(),
        Some(PixelFormat::XRGB8888)
    );
    assert_eq!(client.framebuffer().pixels().len(), 64 * 64 * 4);
    assert_eq!(&client.framebuffer().pixels()[..4], &[255, 255, 255, 0]);
    drop(client);
    server.join().unwrap();
}

#[test]
fn client_reconnects_after_a_server_disconnect() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (stream, peer) = listener.accept().unwrap();
        let first = EncryptedTransportOracle {
            allowed_peer: Some(peer.ip()),
            expect_security_selection: false,
            close_after_frames: Some(1),
            ..EncryptedTransportOracle::default()
        }
        .run(stream, peer)
        .unwrap();

        let (stream, peer) = listener.accept().unwrap();
        let second = EncryptedTransportOracle {
            allowed_peer: Some(peer.ip()),
            expect_security_selection: false,
            ..EncryptedTransportOracle::default()
        }
        .run(stream, peer)
        .unwrap();
        (first, second)
    });

    let mut config =
        ArdClientConfig::new(address.to_string(), b"viewer".to_vec(), b"oracle".to_vec());
    config.reconnect = ArdReconnectPolicy::new(1, std::time::Duration::ZERO);
    let mut client = ArdClient::connect(config).unwrap();
    assert_eq!(client.next_frame().unwrap().index, 1);
    assert_eq!(client.next_frame().unwrap().index, 1);

    drop(client);
    let (first, second) = server.join().unwrap();
    assert_eq!(first.frames_sent, 1);
    assert!(second.activation_received);
}

#[test]
fn encrypted_client_input_sends_keyboard_pointer_and_clipboard_messages() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (stream, peer) = listener.accept().unwrap();
        EncryptedTransportOracle {
            allowed_peer: Some(peer.ip()),
            expect_security_selection: false,
            ..EncryptedTransportOracle::default()
        }
        .run(stream, peer)
        .unwrap()
    });

    let mut client = ArdClient::connect(ArdClientConfig::new(
        address.to_string(),
        b"viewer".to_vec(),
        b"oracle".to_vec(),
    ))
    .unwrap();
    client.next_frame().unwrap();
    let input = client.input();
    input.send_key_event(true, 0x61).unwrap();
    input.send_key_event(false, 0x61).unwrap();
    input.send_pointer_event(0x01, 31, 29).unwrap();
    input.send_pointer_event(0, 31, 29).unwrap();
    input.send_clipboard_text("from viewer").unwrap();
    client.next_frame().unwrap();

    drop(input);
    drop(client);
    let report = server.join().unwrap();
    assert!(report.client_message_types.contains(&4));
    assert!(report.client_message_types.contains(&5));
    assert!(report.client_message_types.contains(&6));
}

#[test]
fn next_event_delivers_server_clipboard_text() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (stream, peer) = listener.accept().unwrap();
        EncryptedTransportOracle {
            allowed_peer: Some(peer.ip()),
            expect_security_selection: false,
            server_clipboard_text: Some(b"from remote".to_vec()),
            ..EncryptedTransportOracle::default()
        }
        .run(stream, peer)
        .unwrap()
    });

    let mut client = ArdClient::connect(ArdClientConfig::new(
        address.to_string(),
        b"viewer".to_vec(),
        b"oracle".to_vec(),
    ))
    .unwrap();
    assert!(matches!(
        client.next_event().unwrap(),
        ArdClientEvent::Frame(_)
    ));
    assert_eq!(
        client.next_event().unwrap(),
        ArdClientEvent::Clipboard("from remote".to_owned())
    );

    drop(client);
    server.join().unwrap();
}
