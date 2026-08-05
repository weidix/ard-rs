#![cfg(feature = "viewer")]

use std::net::TcpListener;
use std::thread;

use ard_rs::{
    ArdClient, ArdClientConfig, ArdClientEvent, ArdVideoQuality, EncryptedTransportOracle,
    MvsGpuTile,
};

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
    assert!(client.framebuffer().rgba().is_empty());
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
fn full_quality_client_negotiates_lossless_zlib_and_updates_rgba() {
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
    assert_eq!(client.framebuffer().rgba().len(), 64 * 64 * 4);
    assert_eq!(&client.framebuffer().rgba()[..4], &[255, 255, 255, 255]);
    assert!(client.take_gpu_mvs_frames().is_empty());

    let second = client.next_frame().unwrap();
    assert_eq!(second.index, 2);
    assert_eq!(&client.framebuffer().rgba()[..4], &[32, 96, 192, 255]);

    drop(client);
    let report = server.join().unwrap();
    assert_eq!(report.viewer_encodings, [6, 16, -223]);
    assert_eq!(report.client_message_types, [3, 9]);
    assert_eq!(report.client_framebuffer_update_incremental, [false]);
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
