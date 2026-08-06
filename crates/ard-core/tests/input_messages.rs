use ard_rs::{
    ArdKey, ArdNamedKey, ArdScrollWheelEvent, build_ard_scroll_wheel_event, build_client_cut_text,
    build_clipboard_text, build_key_event, build_pointer_event, keysym_for_key,
};

#[test]
fn builds_standard_rfb_keyboard_and_pointer_messages() {
    assert_eq!(
        build_key_event(true, keysym_for_key(ArdKey::Character('中')).unwrap()),
        [4, 1, 0, 0, 0x01, 0x00, 0x4e, 0x2d]
    );
    assert_eq!(
        build_key_event(false, 0xffe1),
        [4, 0, 0, 0, 0, 0, 0xff, 0xe1]
    );
    assert_eq!(
        build_pointer_event(0x05, 0x1234, 0xabcd),
        [5, 0x05, 0x12, 0x34, 0xab, 0xcd]
    );
}

#[test]
fn builds_native_apple_scroll_wheel_message() {
    let message = build_ard_scroll_wheel_event(ArdScrollWheelEvent {
        delta_x: -1,
        delta_y: 2,
        fixed_delta_x: -65_536,
        fixed_delta_y: 131_072,
        point_delta_x: -3,
        point_delta_y: 7,
        scroll_phase: 4,
        momentum_phase: 8,
        scroll_count: 9,
        flags: 0x0012_0000,
        x: 0x1234,
        y: 0xabcd,
        ..ArdScrollWheelEvent::default()
    });

    assert_eq!(&message[..8], &[0x17, 0, 0, 0x36, 0, 1, 0, 11]);
    assert_eq!(&message[8..14], &[0xff, 0xff, 0, 2, 0, 0]);
    assert_eq!(
        i32::from_be_bytes(message[14..18].try_into().unwrap()),
        -65_536
    );
    assert_eq!(
        i32::from_be_bytes(message[18..22].try_into().unwrap()),
        131_072
    );
    assert_eq!(i32::from_be_bytes(message[26..30].try_into().unwrap()), -3);
    assert_eq!(i32::from_be_bytes(message[30..34].try_into().unwrap()), 7);
    assert_eq!(u32::from_be_bytes(message[38..42].try_into().unwrap()), 4);
    assert_eq!(u32::from_be_bytes(message[42..46].try_into().unwrap()), 8);
    assert_eq!(u32::from_be_bytes(message[46..50].try_into().unwrap()), 9);
    assert_eq!(
        u32::from_be_bytes(message[50..54].try_into().unwrap()),
        0x0012_0000
    );
    assert_eq!(&message[54..], &[0x12, 0x34, 0xab, 0xcd]);
}

#[test]
fn builds_utf8_clipboard_messages_with_exact_length() {
    let message = build_clipboard_text("hello 中").unwrap();
    assert_eq!(&message[..4], &[6, 0, 0, 0]);
    assert_eq!(
        u32::from_be_bytes(message[4..8].try_into().unwrap()) as usize,
        "hello 中".len()
    );
    assert_eq!(&message[8..], "hello 中".as_bytes());
    assert_eq!(
        build_client_cut_text(b"raw").unwrap(),
        vec![6, 0, 0, 0, 0, 0, 0, 3, b'r', b'a', b'w']
    );
}

#[test]
fn maps_named_keys_without_host_scan_codes() {
    assert_eq!(
        keysym_for_key(ArdKey::Named(ArdNamedKey::ArrowLeft)),
        Some(0xff51)
    );
    assert_eq!(
        keysym_for_key(ArdKey::Named(ArdNamedKey::Function(12))),
        Some(0xffc9)
    );
    assert_eq!(
        keysym_for_key(ArdKey::Named(ArdNamedKey::Numpad(7))),
        Some(0xffb7)
    );
}
