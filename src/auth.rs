use aes::Aes128;
use aes::cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};
use md5::{Digest, Md5};
use num_bigint::BigUint;

use crate::{ArdAuthChallenge, ArdAuthResponse, Error, Result};

/// Result of the client side of Apple security type 30.
///
/// The response is safe to place on the wire. The authentication value is
/// retained only so the subsequent 1103 control message can be unwrapped.
pub struct ArdType30ClientExchange {
    response: ArdAuthResponse,
    authentication_value: [u8; 16],
}

impl core::fmt::Debug for ArdType30ClientExchange {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("ArdType30ClientExchange")
            .field("response", &"<redacted>")
            .field("authentication_value", &"<redacted>")
            .finish()
    }
}

impl ArdType30ClientExchange {
    pub fn response(&self) -> &ArdAuthResponse {
        &self.response
    }

    pub fn into_parts(self) -> (ArdAuthResponse, [u8; 16]) {
        (self.response, self.authentication_value)
    }
}

/// Builds the type-30 response using caller-supplied random bytes.
///
/// `private_random` must be exactly twice the negotiated modulus width, which
/// matches the installed Screen Sharing client. `credential_noise` fills the
/// unused bytes in the two fixed 64-byte credential fields.
pub fn build_ard_type30_client_exchange(
    challenge: &ArdAuthChallenge,
    username: &[u8],
    password: &[u8],
    private_random: &[u8],
    mut credential_noise: [u8; 128],
    max_modulus_bytes: usize,
) -> Result<ArdType30ClientExchange> {
    let width = challenge.prime.len();
    if width == 0 {
        return Err(Error::Invalid("empty ARD authentication modulus"));
    }
    if width > max_modulus_bytes {
        return Err(Error::LimitExceeded("ARD authentication modulus"));
    }
    if challenge.server_public_key.len() != width {
        return Err(Error::Invalid("mismatched ARD authentication parameters"));
    }
    let expected_random_len = width
        .checked_mul(2)
        .ok_or(Error::LimitExceeded("ARD authentication random input"))?;
    if private_random.len() != expected_random_len {
        return Err(Error::Invalid(
            "invalid ARD authentication random input length",
        ));
    }
    validate_credential_field(username)?;
    validate_credential_field(password)?;

    let modulus = BigUint::from_bytes_be(&challenge.prime);
    let generator = BigUint::from(challenge.generator);
    let server_public = BigUint::from_bytes_be(&challenge.server_public_key);
    let private = BigUint::from_bytes_be(private_random);
    let one = BigUint::from(1_u8);
    if modulus <= BigUint::from(3_u8) {
        return Err(Error::Invalid("invalid ARD authentication modulus"));
    }
    let upper = &modulus - &one;
    if generator <= one || generator >= upper {
        return Err(Error::Invalid("invalid ARD authentication generator"));
    }
    if server_public <= one || server_public >= upper {
        return Err(Error::Invalid("invalid ARD server public value"));
    }
    if private == BigUint::from(0_u8) {
        return Err(Error::Invalid("invalid ARD private random input"));
    }

    let client_public = generator.modpow(&private, &modulus);
    let shared = server_public.modpow(&private, &modulus);
    let client_public_key = fixed_width_big_endian(&client_public, width)?;
    let mut shared_bytes = fixed_width_big_endian(&shared, width)?;
    let authentication_value: [u8; 16] = Md5::digest(&shared_bytes).into();
    shared_bytes.fill(0);

    write_credential_field(&mut credential_noise[..64], username);
    write_credential_field(&mut credential_noise[64..], password);
    let cipher = Aes128::new(GenericArray::from_slice(&authentication_value));
    for block in credential_noise.chunks_exact_mut(16) {
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
    }

    Ok(ArdType30ClientExchange {
        response: ArdAuthResponse {
            encrypted_credentials: credential_noise,
            client_public_key,
        },
        authentication_value,
    })
}

fn validate_credential_field(value: &[u8]) -> Result<()> {
    if value.len() >= 64 {
        return Err(Error::LimitExceeded("ARD credential field"));
    }
    if value.contains(&0) {
        return Err(Error::Invalid("ARD credential field contains NUL"));
    }
    Ok(())
}

fn write_credential_field(destination: &mut [u8], value: &[u8]) {
    destination[..value.len()].copy_from_slice(value);
    destination[value.len()] = 0;
}

fn fixed_width_big_endian(value: &BigUint, width: usize) -> Result<Vec<u8>> {
    let encoded = value.to_bytes_be();
    if encoded.len() > width {
        return Err(Error::Invalid(
            "ARD integer does not fit negotiated modulus width",
        ));
    }
    let padding = width - encoded.len();
    let mut result = vec![0; width];
    result[padding..].copy_from_slice(&encoded);
    Ok(result)
}
