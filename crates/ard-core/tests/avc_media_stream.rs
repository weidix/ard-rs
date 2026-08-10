//! End-to-end tests for the AVC media stream negotiation wire format.

use ard_rs::avc::{
    CLIENT_MEDIA_STREAM_MESSAGE_TYPE, ENCODING_AVC_MEDIA_STREAM, MediaStreamAnswer,
    MediaStreamConfiguration, MediaStreamFlags, MediaStreamKeyMaterial, MediaStreamMessage1,
    MediaStreamOffer, MediaStreamServerReply, SERVER_MEDIA_STREAM_MESSAGE_TYPE,
    build_media_stream_offer, build_remote_endpoint_info,
};

#[cfg(target_os = "macos")]
use std::io::Write;

fn sample_offer() -> MediaStreamConfiguration {
    let audio_offer = vec![0x61; 46]; // binary plist placeholder
    let video1_offer = vec![0x76; 46];
    let video2_offer = vec![0x77; 46];
    let keys = MediaStreamKeyMaterial::new(
        &[0x01; 46],
        &[0x02; 46],
        &[0x03; 46],
        &[0x04; 46],
        Some(&[0x05; 46]),
        Some(&[0x06; 46]),
    )
    .expect("keys");
    MediaStreamConfiguration {
        message_version: 0x0300,
        flags: MediaStreamFlags::new(
            MediaStreamFlags::VIDEO1_60FPS
                | MediaStreamFlags::VIDEO2_60FPS
                | MediaStreamFlags::SEND_CURSOR
                | MediaStreamFlags::VIEWER_APP,
        ),
        session_id: [0x11; 16],
        audio_offer,
        video1_offer,
        video2_offer: Some(video2_offer),
        keys,
    }
}

#[test]
fn offer_round_trips_and_matches_native_size() {
    let offer = sample_offer();
    let bytes = offer.encode().expect("encodes");
    assert_eq!(bytes.len(), 450);
    assert_eq!(bytes[0], CLIENT_MEDIA_STREAM_MESSAGE_TYPE);
    // messageSize field = total - 4
    assert_eq!(u16::from_be_bytes([bytes[2], bytes[3]]), 446);
    assert_eq!(u16::from_be_bytes([bytes[4], bytes[5]]), 0x0300);
    assert_eq!(u32::from_be_bytes(bytes[6..10].try_into().unwrap()), 0x0f);

    let (parsed, consumed) = MediaStreamConfiguration::parse(&bytes).expect("parses");
    assert_eq!(consumed, 450);
    assert_eq!(parsed, offer);
    assert_eq!(parsed.keys.audio_viewer_to_server, [0x01; 46]);
    assert_eq!(parsed.keys.audio_server_to_viewer, [0x02; 46]);
    assert_eq!(parsed.keys.video1_viewer_to_server, [0x03; 46]);
    assert_eq!(parsed.keys.video1_server_to_viewer, [0x04; 46]);
    assert_eq!(parsed.keys.video2_viewer_to_server, Some([0x05; 46]));
    assert_eq!(parsed.keys.video2_server_to_viewer, Some([0x06; 46]));
}

#[test]
fn offer_without_video2_round_trips() {
    let mut offer = sample_offer();
    offer.video2_offer = None;
    offer.keys.video2_viewer_to_server = None;
    offer.keys.video2_server_to_viewer = None;
    let bytes = offer.encode().expect("encodes");
    assert_eq!(bytes.len(), 312);
    let (parsed, consumed) = MediaStreamConfiguration::parse(&bytes).expect("parses");
    assert_eq!(consumed, 312);
    assert_eq!(parsed, offer);
}

#[test]
fn message1_round_trips() {
    let message = MediaStreamMessage1 {
        encoding: ENCODING_AVC_MEDIA_STREAM,
        video1_port: 5901,
        video2_port: Some(5902),
        audio_port: Some(5903),
        video1_hdr: true,
        video2_hdr: false,
        stream_count: 1,
    };
    let bytes = message.encode();
    let parsed = MediaStreamMessage1::parse(&bytes).expect("parses");
    assert_eq!(parsed.encoding, ENCODING_AVC_MEDIA_STREAM);
    assert_eq!(parsed.video1_port, 5901);
    assert_eq!(parsed.video2_port, Some(5902));
    assert_eq!(parsed.audio_port, Some(5903));
    assert!(parsed.video1_hdr);
    assert!(!parsed.video2_hdr);
}

#[test]
fn server_reply_classification() {
    let message1 = MediaStreamMessage1 {
        encoding: ENCODING_AVC_MEDIA_STREAM,
        video1_port: 5901,
        video2_port: Some(5902),
        audio_port: Some(5903),
        video1_hdr: false,
        video2_hdr: false,
        stream_count: 1,
    };
    let reply = MediaStreamServerReply::parse(SERVER_MEDIA_STREAM_MESSAGE_TYPE, &message1.encode())
        .expect("classifies message1");
    assert!(matches!(reply, MediaStreamServerReply::Message1(_)));
}

#[test]
fn answer_round_trips_without_overwriting_its_header() {
    let answer = MediaStreamAnswer {
        flags: 0x0102_0304,
        field_a: 123,
        field_b: 456,
        field_c: 789,
        answer_body: b"bplist00answer".to_vec(),
    };
    let bytes = answer.encode().expect("encodes");
    let parsed = MediaStreamAnswer::parse(&bytes).expect("parses");
    assert_eq!(parsed, answer);
}

#[test]
fn server_answer_is_not_misclassified_as_message1() {
    let answer = MediaStreamAnswer {
        flags: 0x0102_0304,
        field_a: 1,
        field_b: 2,
        field_c: 3,
        answer_body: b"bplist00answer".to_vec(),
    };
    let body = answer.encode().expect("encodes");
    let reply = MediaStreamServerReply::parse(SERVER_MEDIA_STREAM_MESSAGE_TYPE, &body)
        .expect("classifies answer");
    assert!(matches!(reply, MediaStreamServerReply::Answer(_)));

    let mut framed = vec![SERVER_MEDIA_STREAM_MESSAGE_TYPE];
    framed.extend_from_slice(&body);
    let (reply, consumed) = MediaStreamServerReply::parse_framed(&framed).expect("frames answer");
    assert_eq!(consumed, framed.len());
    assert!(matches!(reply, MediaStreamServerReply::Answer(_)));
}

#[test]
fn server_error_round_trips_and_frames() {
    let error = ard_rs::avc::MediaStreamError {
        error_type: 3,
        error_sub_code: 7,
    };
    let body = error.encode();
    let reply = MediaStreamServerReply::parse(SERVER_MEDIA_STREAM_MESSAGE_TYPE, &body)
        .expect("classifies error");
    assert_eq!(reply, MediaStreamServerReply::Error(error));

    let mut framed = vec![SERVER_MEDIA_STREAM_MESSAGE_TYPE];
    framed.extend_from_slice(&body);
    let (reply, consumed) = MediaStreamServerReply::parse_framed(&framed).expect("frames error");
    assert_eq!(consumed, framed.len());
    assert_eq!(reply, MediaStreamServerReply::Error(error));
}

#[test]
fn compact_server_message1_envelope_is_supported() {
    let message = MediaStreamMessage1 {
        encoding: ENCODING_AVC_MEDIA_STREAM,
        video1_port: 5901,
        video2_port: None,
        audio_port: None,
        video1_hdr: false,
        video2_hdr: false,
        stream_count: 1,
    };
    let full = message.encode();
    let compact = &full[0x0e..];
    let mut framed = vec![SERVER_MEDIA_STREAM_MESSAGE_TYPE, 0, 0, compact.len() as u8];
    framed.extend_from_slice(compact);
    let (parsed, consumed) = MediaStreamServerReply::parse_framed(&framed).expect("parses");
    assert_eq!(consumed, framed.len());
    assert!(matches!(parsed, MediaStreamServerReply::Message1(_)));
}

#[test]
fn offer_plist_builder_round_trips() {
    let endpoint = build_remote_endpoint_info("Mac16,12", "25G72");
    assert_eq!(
        endpoint,
        vec![
            0x08, 0x00, 0x10, 0x01, 0x1a, 0x08, b'M', b'a', b'c', b'1', b'6', b',', b'1', b'2',
            0x2a, 0x05, b'2', b'5', b'G', b'7', b'2',
        ]
    );
    let offer = build_media_stream_offer("0FE233A5-990F-4B52-B2B2-4FD1BCEB53CC", &endpoint, 7, 2)
        .expect("builds offer");
    assert!(offer.starts_with(b"bplist00"));
    let parsed = MediaStreamOffer::parse(&offer).expect("parses offer plist");
    assert_eq!(
        parsed.call_id.as_deref(),
        Some("0FE233A5-990F-4B52-B2B2-4FD1BCEB53CC")
    );
    assert_eq!(
        parsed.remote_endpoint_info.as_deref(),
        Some(endpoint.as_slice())
    );
    assert_eq!(parsed.mode, Some(7));
    assert_eq!(parsed.direction, Some(2));
}

#[cfg(target_os = "macos")]
#[test]
fn offer_plist_is_accepted_by_foundation() {
    use std::process::{Command, Stdio};

    let endpoint = build_remote_endpoint_info("Mac16,12", "25G72");
    let offer = build_media_stream_offer("0FE233A5-990F-4B52-B2B2-4FD1BCEB53CC", &endpoint, 7, 2)
        .expect("builds offer");
    let mut child = Command::new("plutil")
        .args(["-lint", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("plutil is available on macOS");
    child
        .stdin
        .as_mut()
        .expect("plutil stdin")
        .write_all(&offer)
        .expect("write plist");
    let output = child.wait_with_output().expect("wait for plutil");
    assert!(
        output.status.success(),
        "Foundation rejected generated plist: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
