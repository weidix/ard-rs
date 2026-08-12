use ard_rs::{ArdDisplayConfiguration, ArdVirtualDisplay, build_ard_set_display_configuration};

#[test]
fn builds_fixed_single_display_configuration() {
    let message = build_ard_set_display_configuration(&ArdDisplayConfiguration::single(1440, 900))
        .expect("valid display configuration");

    assert_eq!(message.len(), 308);
    assert_eq!(&message[..12], &[0x1d, 0, 1, 48, 0, 1, 0, 1, 0, 0, 0, 0]);
    assert_eq!(&message[12..14], &296_u16.to_be_bytes());
    assert_eq!(
        f32::from_be_bytes(message[142..146].try_into().expect("physical width")),
        (3840.0_f64 * f64::from_bits(0x3fb8_a15b_8a15_b8a1)) as f32
    );
    assert_eq!(
        f32::from_be_bytes(message[146..150].try_into().expect("physical height")),
        (2160.0_f64 * f64::from_bits(0x3fb8_a15b_8a15_b8a1)) as f32
    );
    assert_eq!(&message[150..154], &3840_u32.to_be_bytes());
    assert_eq!(&message[154..158], &2160_u32.to_be_bytes());
    assert_eq!(&message[166..168], &5_u16.to_be_bytes());
    assert_eq!(&message[168..172], &2880_u32.to_be_bytes());
    assert_eq!(&message[172..176], &1800_u32.to_be_bytes());
    assert_eq!(&message[176..180], &1440_u32.to_be_bytes());
    assert_eq!(&message[180..184], &900_u32.to_be_bytes());
    assert_eq!(&message[184..192], &60.0_f64.to_bits().to_be_bytes());
}

#[test]
fn builds_two_independent_display_records() {
    let configuration = ArdDisplayConfiguration {
        displays: vec![
            ArdVirtualDisplay::named(1920, 1080, "Left"),
            ArdVirtualDisplay::named(1312, 848, "Right"),
        ],
    };
    let message = build_ard_set_display_configuration(&configuration)
        .expect("valid dual-display configuration");

    assert_eq!(message.len(), 604);
    assert_eq!(&message[6..8], &2_u16.to_be_bytes());
    assert_eq!(&message[14..18], b"Left");
    assert_eq!(&message[310..315], b"Right");
    assert_eq!(&message[446..450], &3840_u32.to_be_bytes());
    assert_eq!(&message[450..454], &2160_u32.to_be_bytes());
    assert_eq!(&message[464..468], &2624_u32.to_be_bytes());
    assert_eq!(&message[468..472], &1696_u32.to_be_bytes());
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
    assert!(
        build_ard_set_display_configuration(&ArdDisplayConfiguration::single(2560, 1440)).is_err()
    );
}
