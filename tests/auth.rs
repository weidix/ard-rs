use aes::Aes128;
use aes::cipher::{BlockDecrypt, KeyInit, generic_array::GenericArray};
use ard_rs::{ArdAuthChallenge, Error, build_ard_type30_client_exchange};
use md5::{Digest, Md5};

#[test]
fn builds_type_30_exchange_from_bounded_inputs() {
    // Small arithmetic fixture: p=23, g=5, server exponent=6 gives public 8;
    // client exponent=15 gives public 19 and shared value 2.
    let challenge = ArdAuthChallenge {
        generator: 5,
        prime: vec![23],
        server_public_key: vec![8],
    };
    let exchange = build_ard_type30_client_exchange(
        &challenge,
        b"viewer",
        b"example",
        &[0, 15],
        [0xa5; 128],
        512,
    )
    .unwrap();
    assert_eq!(exchange.response().client_public_key, [19]);

    let (response, authentication_value) = exchange.into_parts();
    let expected: [u8; 16] = Md5::digest([2]).into();
    assert_eq!(authentication_value, expected);

    let cipher = Aes128::new(GenericArray::from_slice(&authentication_value));
    let mut credentials = response.encrypted_credentials;
    for block in credentials.chunks_exact_mut(16) {
        cipher.decrypt_block(GenericArray::from_mut_slice(block));
    }
    assert_eq!(&credentials[..7], b"viewer\0");
    assert_eq!(&credentials[64..72], b"example\0");
    assert!(credentials[7..64].iter().all(|byte| *byte == 0xa5));
    assert!(credentials[72..].iter().all(|byte| *byte == 0xa5));
}

#[test]
fn type_30_exchange_rejects_invalid_or_unbounded_inputs() {
    let challenge = ArdAuthChallenge {
        generator: 5,
        prime: vec![23],
        server_public_key: vec![8],
    };
    assert_eq!(
        build_ard_type30_client_exchange(&challenge, b"viewer", b"example", &[15], [0; 128], 512,)
            .unwrap_err(),
        Error::Invalid("invalid ARD authentication random input length")
    );
    assert_eq!(
        build_ard_type30_client_exchange(
            &challenge,
            &[b'x'; 64],
            b"example",
            &[0, 15],
            [0; 128],
            512,
        )
        .unwrap_err(),
        Error::LimitExceeded("ARD credential field")
    );

    let invalid_public = ArdAuthChallenge {
        server_public_key: vec![1],
        ..challenge
    };
    assert_eq!(
        build_ard_type30_client_exchange(
            &invalid_public,
            b"viewer",
            b"example",
            &[0, 15],
            [0; 128],
            512,
        )
        .unwrap_err(),
        Error::Invalid("invalid ARD server public value")
    );
}

#[test]
fn type_30_exchange_debug_is_redacted() {
    let challenge = ArdAuthChallenge {
        generator: 5,
        prime: vec![23],
        server_public_key: vec![8],
    };
    let exchange =
        build_ard_type30_client_exchange(&challenge, b"u", b"p", &[0, 15], [0; 128], 512).unwrap();
    let output = format!("{exchange:?}");
    assert!(output.contains("<redacted>"));
    assert!(!output.contains("encrypted_credentials"));
}
