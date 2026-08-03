#![cfg(feature = "viewer")]

use std::net::TcpListener;
use std::thread;

use ard_rs::{ArdClient, ArdClientConfig, EncryptedTransportOracle, MvsGpuTile};

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

    drop(client);
    let report = server.join().unwrap();
    assert!(report.activation_received);
    assert!(report.client_message_types.contains(&3));
}
