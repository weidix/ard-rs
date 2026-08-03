use ard_rs::{
    ArdMessageDispatcher, ArdServerMessage, Decoder, Encoding, Error, Framebuffer, PixelFormat,
    Rectangle,
};

fn packed_bits(fields: &[(u32, u8)]) -> Vec<u8> {
    let bit_count: usize = fields.iter().map(|(_, width)| usize::from(*width)).sum();
    let mut bytes = vec![0_u8; bit_count.div_ceil(8)];
    let mut position = 0_usize;
    for &(value, width) in fields {
        assert!(width <= 32);
        for shift in (0..width).rev() {
            if value & (1 << shift) != 0 {
                bytes[position / 8] |= 0x80 >> (position % 8);
            }
            position += 1;
        }
    }
    bytes
}

fn white_mvs_rectangle() -> Vec<u8> {
    let primary = packed_bits(&[
        (0, 1),    // initial state
        (0, 3),    // white tile
        (1, 1),    // extended repeat
        (15, 4),   // repeat base: 16
        (47, 8),   // repeat extension: total repeat count 63
        (0x6d, 8), // primary marker
    ]);
    let secondary = packed_bits(&[(0x6d, 8)]);
    let secondary_offset = 6 + primary.len();
    let mut update = vec![
        0,
        0,
        0,
        ((secondary_offset >> 16) & 0xff) as u8,
        ((secondary_offset >> 8) & 0xff) as u8,
        (secondary_offset & 0xff) as u8,
    ];
    update.extend_from_slice(&primary);
    update.extend_from_slice(&secondary);
    let mut packet = (update.len() as u32).to_be_bytes().to_vec();
    packet.extend_from_slice(&update);
    packet
}

fn framebuffer_update(rectangles: &[(Rectangle, Vec<u8>)]) -> Vec<u8> {
    let mut update = vec![0, 0, 0, 0];
    update[2..4].copy_from_slice(&(rectangles.len() as u16).to_be_bytes());
    for (rect, payload) in rectangles {
        update.extend_from_slice(&rect.x.to_be_bytes());
        update.extend_from_slice(&rect.y.to_be_bytes());
        update.extend_from_slice(&rect.width.to_be_bytes());
        update.extend_from_slice(&rect.height.to_be_bytes());
        update.extend_from_slice(&rect.encoding.to_be_bytes());
        update.extend_from_slice(payload);
    }
    update
}

fn mvs_rect(width: u16, height: u16) -> (Rectangle, Vec<u8>) {
    (
        Rectangle {
            x: 0,
            y: 0,
            width,
            height,
            encoding: Encoding::ArdMvs as i32,
        },
        white_mvs_rectangle(),
    )
}

#[test]
fn dispatcher_routes_mvs_framebuffer_update_across_fragments() {
    let update = framebuffer_update(&[mvs_rect(64, 64)]);
    let mut dispatcher = ArdMessageDispatcher::new(1024 * 1024, 64 * 1024).unwrap();
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(64, 64).unwrap();

    let mut messages = Vec::new();
    for (index, byte) in update.iter().enumerate() {
        messages.extend(
            dispatcher
                .push(&[*byte], &mut decoder, &mut framebuffer)
                .unwrap(),
        );
        if index < update.len() - 1 {
            assert_eq!(dispatcher.buffered_bytes(), index + 1);
        }
    }
    assert_eq!(dispatcher.buffered_bytes(), 0);
    assert_eq!(
        messages,
        [ArdServerMessage::FramebufferUpdate {
            rectangle_count: 1,
            bytes: update.len()
        }]
    );
    assert!(
        framebuffer
            .rgba()
            .chunks_exact(4)
            .all(|pixel| pixel == [255, 255, 255, 255])
    );
}

#[test]
fn dispatcher_exposes_encryption_control_rectangle() {
    let mut control = vec![1_u32.to_be_bytes().to_vec()];
    control.push(vec![0x11; 16]);
    control.push(vec![0x22; 16]);
    let control = control.concat();
    let zero = Rectangle {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
        encoding: Encoding::ArdEncryption as i32,
    };
    let update = framebuffer_update(&[(zero, control)]);

    let mut dispatcher = ArdMessageDispatcher::new(1024, 1024).unwrap();
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(8, 8).unwrap();
    let messages = dispatcher
        .push(&update, &mut decoder, &mut framebuffer)
        .unwrap();
    assert_eq!(messages.len(), 2);
    match &messages[0] {
        ArdServerMessage::FramebufferUpdate {
            rectangle_count: 1, ..
        } => {}
        _ => panic!("expected FramebufferUpdate first"),
    }
    match &messages[1] {
        ArdServerMessage::EncryptionControl(control) => {
            assert_eq!(control.command, 1);
            assert_eq!(control.wrapped_session_blocks()[0], [0x11; 16]);
            assert_eq!(control.wrapped_session_blocks()[1], [0x22; 16]);
        }
        _ => panic!("expected EncryptionControl"),
    }
}

#[test]
fn dispatcher_handles_bell_and_cut_text_with_limits() {
    let mut stream = vec![2];
    let text = "hello from the server";
    let mut cut_text = vec![3, 0, 0, 0];
    cut_text.extend_from_slice(&(text.len() as u32).to_be_bytes());
    cut_text.extend_from_slice(text.as_bytes());
    stream.extend_from_slice(&cut_text);

    let mut dispatcher = ArdMessageDispatcher::new(1024, 1024).unwrap();
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(1, 1).unwrap();
    let messages = dispatcher
        .push(&stream, &mut decoder, &mut framebuffer)
        .unwrap();
    assert_eq!(
        messages,
        [
            ArdServerMessage::Bell,
            ArdServerMessage::ServerCutText(text.to_owned())
        ]
    );

    let mut too_long = vec![3, 0, 0, 0];
    too_long.extend_from_slice(&256_u32.to_be_bytes());
    too_long.extend_from_slice(&[b'x'; 256]);
    let mut dispatcher = ArdMessageDispatcher::new(1024, 64).unwrap();
    assert_eq!(
        dispatcher
            .push(&too_long, &mut decoder, &mut framebuffer)
            .unwrap_err(),
        Error::LimitExceeded("ARD cut-text length")
    );
    assert_eq!(dispatcher.buffered_bytes(), 0);
}

#[test]
fn dispatcher_buffers_partial_cut_text() {
    let mut dispatcher = ArdMessageDispatcher::new(1024, 1024).unwrap();
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(1, 1).unwrap();
    let mut cut_text = vec![3, 0, 0, 0];
    cut_text.extend_from_slice(&4_u32.to_be_bytes());
    cut_text.extend_from_slice(b"abcd");

    assert_eq!(
        dispatcher
            .push(&cut_text[..3], &mut decoder, &mut framebuffer)
            .unwrap(),
        Vec::<ArdServerMessage>::new()
    );
    assert_eq!(dispatcher.buffered_bytes(), 3);
    assert_eq!(
        dispatcher
            .push(&cut_text[3..], &mut decoder, &mut framebuffer)
            .unwrap(),
        [ArdServerMessage::ServerCutText("abcd".to_owned())]
    );
    assert_eq!(dispatcher.buffered_bytes(), 0);
}

#[test]
fn dispatcher_rejects_unsupported_message_types_transactionally() {
    let stream = vec![2, 0x7f];
    let mut dispatcher = ArdMessageDispatcher::new(1024, 1024).unwrap();
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(1, 1).unwrap();
    assert_eq!(
        dispatcher
            .push(&stream, &mut decoder, &mut framebuffer)
            .unwrap_err(),
        Error::Invalid("unsupported ARD server message type")
    );
    // The failed batch was not consumed; the valid Bell alone still parses.
    assert_eq!(dispatcher.buffered_bytes(), 0);
    assert_eq!(
        dispatcher
            .push(&[2], &mut decoder, &mut framebuffer)
            .unwrap(),
        [ArdServerMessage::Bell]
    );
}

#[test]
fn dispatcher_enforces_buffered_message_limit() {
    let mut dispatcher = ArdMessageDispatcher::new(8, 1024).unwrap();
    let mut decoder = Decoder::new(PixelFormat::XRGB8888).unwrap();
    let mut framebuffer = Framebuffer::new(1, 1).unwrap();
    dispatcher
        .push(&[0, 0, 0, 1], &mut decoder, &mut framebuffer)
        .unwrap();
    assert_eq!(
        dispatcher
            .push(&[0; 8], &mut decoder, &mut framebuffer)
            .unwrap_err(),
        Error::LimitExceeded("ARD buffered messages")
    );
    assert_eq!(dispatcher.buffered_bytes(), 4);
}
