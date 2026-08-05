use ard_rs::{
    ArdKey, ArdNamedKey, build_client_cut_text, build_clipboard_text, build_key_event,
    build_pointer_event, keysym_for_key,
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
