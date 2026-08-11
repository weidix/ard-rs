use ard_rs::{ArdDisplayConfiguration, ArdVirtualDisplay, build_ard_set_display_configuration};

#[test]
fn builds_fixed_single_display_configuration() {
    let message = build_ard_set_display_configuration(&ArdDisplayConfiguration::single(2560, 1440))
        .expect("valid display configuration");

    assert_eq!(message.len(), 196);
    assert_eq!(&message[..12], &[0x1d, 0, 0, 192, 0, 1, 0, 1, 0, 0, 0, 0]);
    assert_eq!(&message[12..14], &184_u16.to_be_bytes());
    assert_eq!(&message[134..138], &[0, 0, 0, 0]);
    assert_eq!(&message[150..154], &2560_u32.to_be_bytes());
    assert_eq!(&message[154..158], &1440_u32.to_be_bytes());
    assert_eq!(&message[166..168], &1_u16.to_be_bytes());
    assert_eq!(&message[168..172], &2560_u32.to_be_bytes());
    assert_eq!(&message[172..176], &1440_u32.to_be_bytes());
    assert_eq!(&message[184..192], &60.0_f64.to_bits().to_be_bytes());
}

#[test]
fn builds_two_independent_display_records() {
    let configuration = ArdDisplayConfiguration {
        displays: vec![
            ArdVirtualDisplay::named(1920, 1080, "Left"),
            ArdVirtualDisplay::named(3840, 2160, "Right"),
        ],
    };
    let message = build_ard_set_display_configuration(&configuration)
        .expect("valid dual-display configuration");

    assert_eq!(message.len(), 380);
    assert_eq!(&message[6..8], &2_u16.to_be_bytes());
    assert_eq!(&message[14..18], b"Left");
    assert_eq!(&message[198..203], b"Right");
    assert_eq!(&message[334..338], &3840_u32.to_be_bytes());
    assert_eq!(&message[338..342], &2160_u32.to_be_bytes());
}

#[test]
fn rejects_invalid_display_configurations() {
    assert!(
        build_ard_set_display_configuration(&ArdDisplayConfiguration { displays: vec![] }).is_err()
    );
    assert!(
        build_ard_set_display_configuration(&ArdDisplayConfiguration {
            displays: vec![
                ArdVirtualDisplay::new(800, 600),
                ArdVirtualDisplay::new(800, 600),
                ArdVirtualDisplay::new(800, 600),
            ],
        })
        .is_err()
    );
    assert!(
        build_ard_set_display_configuration(&ArdDisplayConfiguration::single(0, 1080)).is_err()
    );
}
