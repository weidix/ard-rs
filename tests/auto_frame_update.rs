use ard_rs::build_ard_auto_frame_update;

#[test]
fn builds_native_automatic_frame_update_subscription() {
    assert_eq!(
        build_ard_auto_frame_update(0, 0, 0, 1920, 1080),
        [
            9, 0, 0, 1, // type, padding, enabled
            0, 0, 0, 0, // native maximum-rate interval
            0, 0, 0, 0, // x, y
            0x07, 0x80, 0x04, 0x38, // width, height
        ]
    );
    assert_eq!(
        build_ard_auto_frame_update(16, 1, 2, 3, 4),
        [9, 0, 0, 1, 0, 0, 0, 16, 0, 1, 0, 2, 0, 3, 0, 4]
    );
}
