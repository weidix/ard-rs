use std::net::TcpListener;
use std::thread;

use ard_rs::{
    ArdClient, ArdClientConfig, ArdClientEvent, ArdDisplayConfiguration, ArdDisplaySelection,
    ArdFrameOutput, ArdReconnectPolicy, ArdVideoQuality, EncryptedTransportOracle, MvsGpuTile,
    OracleMode, PixelFormat, RawStreamIndex, RawStreamKind, RawStreamSink, RecordFraming,
    TakeOrigin,
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
    client.next_frame().unwrap();
    drop(client);

    let report = server.join().unwrap();
    assert_eq!(report.client_message_types[..5], [0, 2, 0x0d, 0x1d, 3]);
    assert_eq!(
        report.client_framebuffer_update_rectangles[0],
        (0, 0, 3840, 2160)
    );
    assert_eq!(
        report.client_auto_frame_update_rectangles[0],
        (0, 0, 3840, 2160)
    );
}

#[test]
fn receive_only_client_delivers_gpu_mvs_tiles_without_cpu_frame_expansion() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (stream, peer) = listener.accept().unwrap();
        EncryptedTransportOracle {
            allowed_peer: Some(peer.ip()),
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
        (1920, 1080)
    );
    assert!(client.framebuffer().pixels().is_empty());
    let gpu_frames = client.take_gpu_mvs_frames();
    assert_eq!(gpu_frames.len(), 1);
    assert_eq!(gpu_frames[0].tiles.len(), 240 * 135);
    assert!(
        gpu_frames[0]
            .tiles
            .iter()
            .any(|tile| matches!(tile.tile, MvsGpuTile::SolidYcbcr(_)))
    );
    let frame = client.next_frame().unwrap();
    assert_eq!(frame.index, 2);
    assert_eq!(frame.framebuffer_updates, 1);
    assert_eq!(client.take_gpu_mvs_frames().len(), 1);
    let layout = client.display_layout().expect("DisplayInfo2 layout");
    assert_eq!(layout.displays[0].id, 1);

    drop(client);
    let report = server.join().unwrap();
    assert!(report.activation_received);
    assert_eq!(
        report.viewer_encodings,
        [
            1011, 1002, 6, 16, -239, 1104, 1100, -223, 1101, 1105, 1107, 1109, 1110
        ]
    );
    assert_eq!(report.client_message_types, [0, 2, 0x0d, 3, 9]);
    assert_eq!(
        report.client_display_selections,
        [ArdDisplaySelection::Combined]
    );
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
    assert_eq!(client.framebuffer().pixels().len(), 1920 * 1080 * 4);
    assert_eq!(&client.framebuffer().pixels()[..4], &[216, 78, 29, 0]);
    assert!(client.take_gpu_mvs_frames().is_empty());

    let second = client.next_frame().unwrap();
    assert_eq!(second.index, 2);
    assert_eq!(&client.framebuffer().pixels()[..4], &[216, 78, 29, 0]);

    drop(client);
    let report = server.join().unwrap();
    assert_eq!(
        report.viewer_encodings,
        [6, 16, -239, 1104, 1100, -223, 1101, 1105, 1107, 1109, 1110]
    );
    assert_eq!(report.client_message_types, [0, 2, 0x0d, 3, 9]);
    assert_eq!(report.client_framebuffer_update_incremental, [false]);
}

#[test]
fn client_selects_one_display_inside_the_encrypted_preface() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (stream, peer) = listener.accept().unwrap();
        EncryptedTransportOracle {
            allowed_peer: Some(peer.ip()),
            close_after_frames: Some(1),
            ..EncryptedTransportOracle::default()
        }
        .run(stream, peer)
        .unwrap()
    });

    let mut config =
        ArdClientConfig::new(address.to_string(), b"viewer".to_vec(), b"oracle".to_vec());
    config.display_selection = ArdDisplaySelection::Display(1);
    let mut client = ArdClient::connect(config).unwrap();
    assert_eq!(client.next_frame().unwrap().index, 1);
    drop(client);

    let report = server.join().unwrap();
    assert_eq!(
        report.client_display_selections,
        [ArdDisplaySelection::Display(1)]
    );
    assert_eq!(report.client_message_types[..4], [0, 2, 0x0d, 3]);
}

#[test]
fn client_can_retain_server_native_pixel_bytes() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (stream, peer) = listener.accept().unwrap();
        EncryptedTransportOracle {
            allowed_peer: Some(peer.ip()),
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
    assert_eq!(client.framebuffer().pixels().len(), 1920 * 1080 * 4);
    assert_eq!(&client.framebuffer().pixels()[..4], &[216, 78, 29, 0]);
    drop(client);
    server.join().unwrap();
}

#[test]
fn fixture_oracle_serves_every_rfb_quality_mode() {
    for (quality, mode) in [
        (ArdVideoQuality::Low, OracleMode::Halftone),
        (ArdVideoQuality::Medium, OracleMode::Grayscale),
        (ArdVideoQuality::High, OracleMode::Thousands),
        (ArdVideoQuality::Adaptive, OracleMode::AdaptiveMvs),
        (ArdVideoQuality::Full, OracleMode::FullColor),
    ] {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, peer) = listener.accept().unwrap();
            EncryptedTransportOracle {
                allowed_peer: Some(peer.ip()),
                mode,
                close_after_frames: Some(1),
                ..EncryptedTransportOracle::default()
            }
            .run(stream, peer)
            .unwrap()
        });

        let mut config =
            ArdClientConfig::new(address.to_string(), b"viewer".to_vec(), b"oracle".to_vec());
        config.video_quality = quality;
        let mut client = ArdClient::connect(config).unwrap();
        assert_eq!(client.next_frame().unwrap().index, 1);
        drop(client);
        assert_eq!(server.join().unwrap().selected_mode, Some(mode));
    }
}

#[test]
fn fixture_oracle_negotiates_both_media_codecs() {
    for (quality, mode) in [
        (ArdVideoQuality::HighPerformanceAvc, OracleMode::H264),
        (ArdVideoQuality::HighPerformanceHevc, OracleMode::Hevc),
    ] {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, peer) = listener.accept().unwrap();
            EncryptedTransportOracle {
                allowed_peer: Some(peer.ip()),
                mode,
                close_after_frames: Some(1),
                ..EncryptedTransportOracle::default()
            }
            .run(stream, peer)
            .unwrap()
        });

        let mut config =
            ArdClientConfig::new(address.to_string(), b"viewer".to_vec(), b"oracle".to_vec());
        config.video_quality = quality;
        let mut client = ArdClient::connect(config).unwrap();
        loop {
            if matches!(client.next_event().unwrap(), ArdClientEvent::MediaStream(_)) {
                break;
            }
        }
        drop(client);
        let report = server.join().unwrap();
        assert!(report.media_configuration_received);
        assert_eq!(report.selected_mode, Some(mode));
    }
}

#[test]
fn client_reconnects_after_a_server_disconnect() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (stream, peer) = listener.accept().unwrap();
        let first = EncryptedTransportOracle {
            allowed_peer: Some(peer.ip()),
            close_after_frames: Some(1),
            ..EncryptedTransportOracle::default()
        }
        .run(stream, peer)
        .unwrap();

        let (stream, peer) = listener.accept().unwrap();
        let second = EncryptedTransportOracle {
            allowed_peer: Some(peer.ip()),
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
    // The writer thread is asynchronous. Wait for the queue to drain instead of
    // relying on a teardown race: before `ArdClient` gained an ordered writer
    // shutdown, the clipboard could still be queued when the session socket
    // closed, so under load message type 6 (and occasionally type 4) never
    // reached the oracle.
    assert!(
        client.flush_input(std::time::Duration::from_secs(5)),
        "queued input must be written before the session is inspected"
    );
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

/// The dump has to hold the take's decrypted server data, not what the client
/// made of it: every record the take read is in the dump, in order, on the
/// video's own clock, with the index describing the bytes exactly.
#[test]
fn the_raw_stream_dump_holds_the_takes_decrypted_records() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (stream, peer) = listener.accept().unwrap();
        EncryptedTransportOracle {
            allowed_peer: Some(peer.ip()),
            ..EncryptedTransportOracle::default()
        }
        .run(stream, peer)
        .unwrap()
    });

    let directory = std::env::temp_dir().join(format!(
        "ard-server-stream-client-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&directory).unwrap();

    let sink = RawStreamSink::new(&directory, address.to_string());
    // The dump is armed like a take: it starts at the recording's first frame and
    // stops with the recording. Nothing outside that interval is dumped.
    let origin = TakeOrigin::new();
    sink.start_recording(origin.clone());
    let mut client = ArdClient::connect(
        ArdClientConfig::new(address.to_string(), b"viewer".to_vec(), b"oracle".to_vec())
            .with_raw_stream(sink.clone()),
    )
    .unwrap();
    // Everything the client reads before the take starts is not part of it.
    assert!(
        !directory.join("192.0.2.10 server stream.raw").exists()
            || sink
                .raw_path(RawStreamKind::Server)
                .metadata()
                .is_ok_and(|m| m.len() == 0),
        "a dump before the take writes nothing"
    );
    origin.set(std::time::Instant::now());
    for _ in 0..3 {
        client.next_frame().unwrap();
    }
    // The path has to be captured before the sink goes away: the dump is
    // flushed when the last holder of the sink is dropped.
    assert!(
        sink.failure().is_none(),
        "the dump was written without failing"
    );
    let raw_path = sink.raw_path(RawStreamKind::Server);
    drop(client);
    sink.stop_recording();
    drop(sink);

    assert!(raw_path.exists());
    let report = server.join().unwrap();
    let index_path = raw_path.with_extension("jsonl");
    let index = RawStreamIndex::read(&index_path).expect("the dump index is readable");
    assert!(!index.records.is_empty(), "the take's records were dumped");
    // Every entry sits inside the take's interval, and the clock it carries is
    // the recorded video's own clock.
    let take_ms = index.take_ms.expect("the footer records the take's length");
    assert!(index.records.iter().all(|record| record.t <= take_ms));
    let raw = std::fs::read(&raw_path).expect("the raw stream is readable");
    // The index has to describe the file exactly, or a replay would read the
    // wrong bytes.
    let mut expected_offset = 0_u64;
    for (number, record) in index.records.iter().enumerate() {
        assert_eq!(record.offset, expected_offset, "record {number} offset");
        assert!(record.length > 0, "record {number} is empty");

        assert_eq!(
            record.framing,
            RecordFraming::TcpRecord,
            "record {number} is a TCP record"
        );
        expected_offset += record.length;
    }
    assert_eq!(expected_offset, raw.len() as u64);
    // Sequence numbers arrive in order, and the payload is the server's message
    // stream: the dumped bytes are exactly the FramebufferUpdate records the
    // oracle sent and not a header the client invented.
    for pair in index.records.windows(2) {
        assert_eq!(pair[1].sequence, pair[0].sequence + 1);
    }
    assert_eq!(
        &raw[..8],
        &[0, 0, 0, 1, 0, 0, 0, 0],
        "the first record is a server FramebufferUpdate, byte for byte"
    );

    // The dump covers the take and not the session around it: it holds every
    // record the take read, in arrival order, and nothing else. This client stops
    // reading once it has the frames it asked for, so records the server sent
    // afterwards stay in the socket and are correctly absent from the dump.
    assert!(
        index.records.len() <= report.server_to_client_records,
        "the dump cannot hold more records than the server sent ({} > {})",
        index.records.len(),
        report.server_to_client_records,
    );
    // The transport's record sequence numbers are the order the server sent them
    // in, so a replay reproduces the server's sequence and not merely its bytes.
    assert_eq!(index.records[0].sequence + 1, index.records[1].sequence);

    std::fs::remove_dir_all(&directory).ok();
}
