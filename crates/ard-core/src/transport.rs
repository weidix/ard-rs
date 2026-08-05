use aes::Aes128;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit, generic_array::GenericArray};
use sha1::{Digest, Sha1};
use subtle::ConstantTimeEq;

use crate::{ArdEncryptionControl, Error, Result};

/// Incremental parser for the encrypted record stream enabled by encoding
/// 1103. Each record is a big-endian `u16` ciphertext length followed by that
/// many bytes. Ciphertexts are non-empty AES block multiples.
#[derive(Debug, Clone)]
pub struct ArdEncryptedRecordFramer {
    max_record_bytes: usize,
    max_records_per_push: usize,
    prefix: [u8; 2],
    prefix_len: usize,
    expected_len: Option<usize>,
    body: Vec<u8>,
}

impl ArdEncryptedRecordFramer {
    pub fn new(max_record_bytes: usize, max_records_per_push: usize) -> Result<Self> {
        if max_record_bytes < 16 || max_record_bytes > usize::from(u16::MAX) {
            return Err(Error::Invalid("invalid encrypted-record size limit"));
        }
        if max_records_per_push == 0 {
            return Err(Error::Invalid("invalid encrypted-record count limit"));
        }
        Ok(Self {
            max_record_bytes,
            max_records_per_push,
            prefix: [0; 2],
            prefix_len: 0,
            expected_len: None,
            body: Vec::new(),
        })
    }

    /// Feeds an arbitrary TCP fragment. State changes are transactional: an
    /// invalid length or per-call record-count overflow leaves the prior
    /// partial record intact.
    pub fn push(&mut self, input: &[u8]) -> Result<Vec<Vec<u8>>> {
        let mut next = self.clone();
        let records = next.push_inner(input)?;
        *self = next;
        Ok(records)
    }

    pub fn buffered_bytes(&self) -> usize {
        self.prefix_len.saturating_add(self.body.len())
    }

    pub fn expected_ciphertext_len(&self) -> Option<usize> {
        self.expected_len
    }

    fn push_inner(&mut self, mut input: &[u8]) -> Result<Vec<Vec<u8>>> {
        let mut records = Vec::new();
        while !input.is_empty() {
            if self.expected_len.is_none() {
                let take = (2 - self.prefix_len).min(input.len());
                self.prefix[self.prefix_len..self.prefix_len + take]
                    .copy_from_slice(&input[..take]);
                self.prefix_len += take;
                input = &input[take..];
                if self.prefix_len != 2 {
                    continue;
                }
                let length = usize::from(u16::from_be_bytes(self.prefix));
                if length == 0 || !length.is_multiple_of(16) {
                    return Err(Error::Invalid(
                        "encrypted-record length is not an AES block multiple",
                    ));
                }
                if length > self.max_record_bytes {
                    return Err(Error::LimitExceeded("encrypted record"));
                }
                self.expected_len = Some(length);
                self.body = Vec::with_capacity(length);
            }

            let expected = self
                .expected_len
                .expect("record length established before body");
            let remaining = expected - self.body.len();
            let take = remaining.min(input.len());
            self.body.extend_from_slice(&input[..take]);
            input = &input[take..];
            if self.body.len() == expected {
                if records.len() == self.max_records_per_push {
                    return Err(Error::LimitExceeded("encrypted records per input"));
                }
                records.push(core::mem::take(&mut self.body));
                self.prefix = [0; 2];
                self.prefix_len = 0;
                self.expected_len = None;
            }
        }
        Ok(records)
    }
}

/// The CBC session value and initial chaining value carried by the 1103
/// control rectangle after its two blocks have been unwrapped.
#[derive(Clone, PartialEq, Eq)]
pub struct ArdSessionMaterial {
    session_value: [u8; 16],
    initial_chaining_value: [u8; 16],
}

impl core::fmt::Debug for ArdSessionMaterial {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("ArdSessionMaterial")
            .field("session_value", &"<redacted>")
            .field("initial_chaining_value", &"<redacted>")
            .finish()
    }
}

impl ArdSessionMaterial {
    pub fn new(session_value: [u8; 16], initial_chaining_value: [u8; 16]) -> Self {
        Self {
            session_value,
            initial_chaining_value,
        }
    }

    pub fn record_decoder(&self, max_plaintext_bytes: usize) -> Result<ArdSessionRecordDecoder> {
        ArdSessionRecordDecoder::new_with_initial_chaining_value(
            self.session_value,
            self.initial_chaining_value,
            max_plaintext_bytes,
        )
    }

    pub fn record_encoder(&self, max_plaintext_bytes: usize) -> Result<ArdSessionRecordEncoder> {
        ArdSessionRecordEncoder::new_with_initial_chaining_value(
            self.session_value,
            self.initial_chaining_value,
            max_plaintext_bytes,
        )
    }
}

pub fn unwrap_ard_session_material(
    control: &ArdEncryptionControl,
    authentication_value: [u8; 16],
) -> ArdSessionMaterial {
    let cipher = Aes128::new(GenericArray::from_slice(&authentication_value));
    let mut parameters = *control.wrapped_session_blocks();
    for bytes in &mut parameters {
        let block = GenericArray::from_mut_slice(bytes);
        cipher.decrypt_block(block);
    }
    ArdSessionMaterial {
        session_value: parameters[0],
        initial_chaining_value: parameters[1],
    }
}

/// Stateful decoder for the persistent AES-CBC stream used after encoding
/// 1103. A record becomes visible only after its encrypted SHA-1 checksum has
/// been verified.
#[derive(Clone)]
pub struct ArdSessionRecordDecoder {
    cipher: Aes128,
    chaining_value: [u8; 16],
    sequence: u32,
    exhausted: bool,
    max_plaintext_bytes: usize,
}

impl core::fmt::Debug for ArdSessionRecordDecoder {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("ArdSessionRecordDecoder")
            .field("sequence", &self.sequence)
            .field("exhausted", &self.exhausted)
            .field("max_plaintext_bytes", &self.max_plaintext_bytes)
            .finish_non_exhaustive()
    }
}

impl ArdSessionRecordDecoder {
    pub fn new(session_value: [u8; 16], max_plaintext_bytes: usize) -> Result<Self> {
        Self::new_with_initial_chaining_value(session_value, [0; 16], max_plaintext_bytes)
    }

    pub fn new_with_initial_chaining_value(
        session_value: [u8; 16],
        initial_chaining_value: [u8; 16],
        max_plaintext_bytes: usize,
    ) -> Result<Self> {
        if max_plaintext_bytes > usize::from(u16::MAX) {
            return Err(Error::Invalid("invalid session plaintext limit"));
        }
        Ok(Self {
            cipher: Aes128::new(GenericArray::from_slice(&session_value)),
            chaining_value: initial_chaining_value,
            sequence: 0,
            exhausted: false,
            max_plaintext_bytes,
        })
    }

    pub fn sequence(&self) -> u32 {
        self.sequence
    }

    pub fn decode(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        let mut plaintext = ciphertext.to_vec();
        self.decode_in_place(&mut plaintext)?;
        Ok(plaintext)
    }

    /// Decodes one complete record in the caller-owned buffer.
    ///
    /// The live client reads exactly one record at a time and terminates the
    /// session on a framing or checksum error. Reusing that buffer avoids a
    /// second allocation and copy for every encrypted record while retaining
    /// the transactional state update: the CBC chain and sequence advance
    /// only after all validation succeeds.
    pub(crate) fn decode_in_place(&mut self, plaintext: &mut Vec<u8>) -> Result<()> {
        if self.exhausted {
            return Err(Error::Invalid("ARD session sequence exhausted"));
        }
        if plaintext.len() < 32 || !plaintext.len().is_multiple_of(16) {
            return Err(Error::Invalid("invalid encrypted-record ciphertext length"));
        }
        let next_chaining_value = cbc_decrypt(&self.cipher, self.chaining_value, plaintext);
        let checksum_offset = plaintext
            .len()
            .checked_sub(20)
            .ok_or(Error::Invalid("invalid encrypted-record plaintext"))?;
        let expected = record_checksum(self.sequence, &plaintext[..checksum_offset]);
        if expected.ct_eq(&plaintext[checksum_offset..]).unwrap_u8() != 1 {
            return Err(Error::Invalid("encrypted-record checksum mismatch"));
        }
        let payload_len = usize::from(u16::from_be_bytes([plaintext[0], plaintext[1]]));
        if payload_len > self.max_plaintext_bytes {
            return Err(Error::LimitExceeded("encrypted-record plaintext"));
        }
        let payload_end = payload_len
            .checked_add(2)
            .ok_or(Error::LimitExceeded("encrypted-record plaintext"))?;
        if payload_end > checksum_offset {
            return Err(Error::Invalid("invalid encrypted-record payload length"));
        }

        // The checksum has already been verified. Compact the payload in the
        // decrypted record buffer instead of allocating a second Vec and
        // copying the payload into it.
        plaintext.copy_within(2..payload_end, 0);
        plaintext.truncate(payload_len);
        self.chaining_value = next_chaining_value;
        if self.sequence == u32::MAX {
            self.exhausted = true;
        } else {
            self.sequence += 1;
        }
        Ok(())
    }
}

/// Stateful encoder matching `ArdSessionRecordDecoder`. It is used by the
/// client-to-server side and by interoperability tests.
#[derive(Clone)]
pub struct ArdSessionRecordEncoder {
    cipher: Aes128,
    chaining_value: [u8; 16],
    sequence: u32,
    exhausted: bool,
    max_plaintext_bytes: usize,
}

impl core::fmt::Debug for ArdSessionRecordEncoder {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("ArdSessionRecordEncoder")
            .field("sequence", &self.sequence)
            .field("exhausted", &self.exhausted)
            .field("max_plaintext_bytes", &self.max_plaintext_bytes)
            .finish_non_exhaustive()
    }
}

impl ArdSessionRecordEncoder {
    pub fn new(session_value: [u8; 16], max_plaintext_bytes: usize) -> Result<Self> {
        Self::new_with_initial_chaining_value(session_value, [0; 16], max_plaintext_bytes)
    }

    pub fn new_with_initial_chaining_value(
        session_value: [u8; 16],
        initial_chaining_value: [u8; 16],
        max_plaintext_bytes: usize,
    ) -> Result<Self> {
        if max_plaintext_bytes > usize::from(u16::MAX) {
            return Err(Error::Invalid("invalid session plaintext limit"));
        }
        Ok(Self {
            cipher: Aes128::new(GenericArray::from_slice(&session_value)),
            chaining_value: initial_chaining_value,
            sequence: 0,
            exhausted: false,
            max_plaintext_bytes,
        })
    }

    pub fn sequence(&self) -> u32 {
        self.sequence
    }

    pub fn encode_wire(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        if self.exhausted {
            return Err(Error::Invalid("ARD session sequence exhausted"));
        }
        if payload.len() > self.max_plaintext_bytes {
            return Err(Error::LimitExceeded("encrypted-record plaintext"));
        }
        let unpadded = payload
            .len()
            .checked_add(22)
            .ok_or(Error::LimitExceeded("encrypted-record plaintext"))?;
        let ciphertext_len = unpadded
            .checked_add(15)
            .ok_or(Error::LimitExceeded("encrypted record"))?
            & !15;
        let wire_len =
            u16::try_from(ciphertext_len).map_err(|_| Error::LimitExceeded("encrypted record"))?;
        let checksum_offset = ciphertext_len - 20;
        let payload_len = u16::try_from(payload.len())
            .map_err(|_| Error::LimitExceeded("encrypted-record plaintext"))?;
        let mut wire = vec![0; 2 + ciphertext_len];
        wire[..2].copy_from_slice(&wire_len.to_be_bytes());
        let ciphertext = &mut wire[2..];
        ciphertext[..2].copy_from_slice(&payload_len.to_be_bytes());
        ciphertext[2..2 + payload.len()].copy_from_slice(payload);
        let checksum = record_checksum(self.sequence, &ciphertext[..checksum_offset]);
        ciphertext[checksum_offset..].copy_from_slice(&checksum);

        self.chaining_value = cbc_encrypt(&self.cipher, self.chaining_value, ciphertext);
        if self.sequence == u32::MAX {
            self.exhausted = true;
        } else {
            self.sequence += 1;
        }
        Ok(wire)
    }
}

/// Transactional composition of TCP record framing and verified record
/// decoding. If any complete record in one input fails, neither layer advances.
#[derive(Clone)]
pub struct ArdVerifiedRecordStream {
    framer: ArdEncryptedRecordFramer,
    decoder: ArdSessionRecordDecoder,
}

impl core::fmt::Debug for ArdVerifiedRecordStream {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("ArdVerifiedRecordStream")
            .field("framer", &self.framer)
            .field("decoder", &self.decoder)
            .finish()
    }
}

impl ArdVerifiedRecordStream {
    pub fn new(
        decoder: ArdSessionRecordDecoder,
        max_record_bytes: usize,
        max_records_per_push: usize,
    ) -> Result<Self> {
        Ok(Self {
            framer: ArdEncryptedRecordFramer::new(max_record_bytes, max_records_per_push)?,
            decoder,
        })
    }

    pub fn push(&mut self, input: &[u8]) -> Result<Vec<Vec<u8>>> {
        let mut next = self.clone();
        let ciphertexts = next.framer.push(input)?;
        let mut plaintexts = Vec::with_capacity(ciphertexts.len());
        for ciphertext in ciphertexts {
            plaintexts.push(next.decoder.decode(&ciphertext)?);
        }
        *self = next;
        Ok(plaintexts)
    }

    /// Decodes one already-framed server record without cloning the stream
    /// state or allocating a plaintext buffer. This is intentionally kept
    /// separate from [`Self::push`], whose public API remains transactional
    /// for arbitrary fragmented input.
    pub(crate) fn decode_record_in_place(&mut self, ciphertext: &mut Vec<u8>) -> Result<()> {
        self.decoder.decode_in_place(ciphertext)
    }

    pub fn sequence(&self) -> u32 {
        self.decoder.sequence()
    }

    pub fn buffered_bytes(&self) -> usize {
        self.framer.buffered_bytes()
    }
}

fn record_checksum(sequence: u32, plaintext: &[u8]) -> [u8; 20] {
    let mut digest = Sha1::new();
    digest.update(sequence.to_be_bytes());
    digest.update(plaintext);
    digest.finalize().into()
}

fn cbc_decrypt(cipher: &Aes128, mut previous: [u8; 16], bytes: &mut [u8]) -> [u8; 16] {
    for chunk in bytes.chunks_exact_mut(16) {
        let current: [u8; 16] = chunk.try_into().expect("AES block sized chunk");
        cipher.decrypt_block(GenericArray::from_mut_slice(chunk));
        for (byte, prior) in chunk.iter_mut().zip(previous) {
            *byte ^= prior;
        }
        previous = current;
    }
    previous
}

fn cbc_encrypt(cipher: &Aes128, mut previous: [u8; 16], bytes: &mut [u8]) -> [u8; 16] {
    for chunk in bytes.chunks_exact_mut(16) {
        for (byte, prior) in chunk.iter_mut().zip(previous) {
            *byte ^= prior;
        }
        cipher.encrypt_block(GenericArray::from_mut_slice(chunk));
        previous.copy_from_slice(chunk);
    }
    previous
}

#[cfg(test)]
mod tests {
    use super::{ArdSessionRecordDecoder, ArdSessionRecordEncoder, cbc_decrypt, cbc_encrypt};
    use aes::Aes128;
    use aes::cipher::{KeyInit, generic_array::GenericArray};

    #[test]
    fn cbc_helpers_match_nist_sp_800_38a() {
        let session_value = [
            0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6, 0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf,
            0x4f, 0x3c,
        ];
        let initial = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        let plaintext = [
            0x6b, 0xc1, 0xbe, 0xe2, 0x2e, 0x40, 0x9f, 0x96, 0xe9, 0x3d, 0x7e, 0x11, 0x73, 0x93,
            0x17, 0x2a, 0xae, 0x2d, 0x8a, 0x57, 0x1e, 0x03, 0xac, 0x9c, 0x9e, 0xb7, 0x6f, 0xac,
            0x45, 0xaf, 0x8e, 0x51,
        ];
        let expected = [
            0x76, 0x49, 0xab, 0xac, 0x81, 0x19, 0xb2, 0x46, 0xce, 0xe9, 0x8e, 0x9b, 0x12, 0xe9,
            0x19, 0x7d, 0x50, 0x86, 0xcb, 0x9b, 0x50, 0x72, 0x19, 0xee, 0x95, 0xdb, 0x11, 0x3a,
            0x91, 0x76, 0x78, 0xb2,
        ];
        let cipher = Aes128::new(GenericArray::from_slice(&session_value));
        let mut encrypted = plaintext;
        assert_eq!(
            cbc_encrypt(&cipher, initial, &mut encrypted),
            expected[16..]
        );
        assert_eq!(encrypted, expected);
        assert_eq!(
            cbc_decrypt(&cipher, initial, &mut encrypted),
            expected[16..]
        );
        assert_eq!(encrypted, plaintext);
    }

    #[test]
    fn in_place_record_decode_matches_allocating_decode() {
        let session_value = [0x2b; 16];
        let initial_chaining_value = [0x07; 16];
        let mut encoder = ArdSessionRecordEncoder::new_with_initial_chaining_value(
            session_value,
            initial_chaining_value,
            u16::MAX as usize,
        )
        .unwrap();
        let wire = encoder.encode_wire(b"reused record buffer").unwrap();

        let mut allocating = ArdSessionRecordDecoder::new_with_initial_chaining_value(
            session_value,
            initial_chaining_value,
            u16::MAX as usize,
        )
        .unwrap();
        let expected = allocating.decode(&wire[2..]).unwrap();

        let mut in_place = ArdSessionRecordDecoder::new_with_initial_chaining_value(
            session_value,
            initial_chaining_value,
            u16::MAX as usize,
        )
        .unwrap();
        let mut buffer = wire[2..].to_vec();
        in_place.decode_in_place(&mut buffer).unwrap();

        assert_eq!(buffer, expected);
        assert_eq!(in_place.sequence(), allocating.sequence());
    }
}
