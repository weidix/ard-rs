//! SRTP AES-128-CM decryption (RFC 3711) for the AVC media stream.
//!
//! Each stream has a 46-byte key blob: bytes 0..16 are the 128-bit master
//! key, bytes 16..30 the 112-bit master salt. The remaining 16 bytes are not
//! used by AES-128-CM (they likely feed the integrity layer, which is not
//! required for decoding and is left for a follow-up milestone).
//!
//! Only the RTP payload is encrypted; the 12-byte RTP header stays in
//! cleartext so the receiver can derive the per-packet keystream from
//! `SSRC`, rollover counter and sequence number.

use aes::Aes128;
use aes::cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};

use crate::{Error, Result};

use super::MEDIA_STREAM_KEY_LEN;

const KEY_LEN: usize = 16;
const SALT_LEN: usize = 14;

/// SRTP decryption context for one media stream.
#[derive(Debug, Clone)]
pub struct SrtpContext {
    cipher: Aes128,
    salt: [u8; SALT_LEN],
    ssrc: Option<u32>,
    /// Rollover counter; advances when the 16-bit sequence wraps.
    roc: u32,
    last_sequence: Option<u16>,
}

impl SrtpContext {
    /// Build a context from a 46-byte negotiated key blob.
    pub fn from_key_blob(blob: &[u8]) -> Result<Self> {
        if blob.len() != MEDIA_STREAM_KEY_LEN {
            return Err(Error::Invalid("SRTP key blob must be 46 bytes"));
        }
        let mut key = [0u8; KEY_LEN];
        let mut salt = [0u8; SALT_LEN];
        key.copy_from_slice(&blob[0..KEY_LEN]);
        salt.copy_from_slice(&blob[KEY_LEN..KEY_LEN + SALT_LEN]);
        Ok(Self {
            cipher: Aes128::new(GenericArray::from_slice(&key)),
            salt,
            ssrc: None,
            roc: 0,
            last_sequence: None,
        })
    }

    /// Reset the sequence tracker (e.g. after an SSRC change).
    pub fn reset(&mut self) {
        self.ssrc = None;
        self.roc = 0;
        self.last_sequence = None;
    }

    /// Decrypt an RTP packet payload in place.
    ///
    /// `ssrc`/`sequence` come from the cleartext RTP header; `payload` must
    /// be the encrypted bytes that follow the header. Returns a newly
    /// decrypted copy.
    pub fn decrypt_rtp_payload(
        &mut self,
        ssrc: u32,
        sequence: u16,
        payload: &[u8],
    ) -> Result<Vec<u8>> {
        if payload.is_empty() {
            return Ok(Vec::new());
        }
        match self.ssrc {
            Some(known) if known != ssrc => {
                self.reset();
                self.ssrc = Some(ssrc);
            }
            None => self.ssrc = Some(ssrc),
            _ => {}
        }
        let packet_roc = self.guess_roc(sequence)?;
        let index = (u64::from(packet_roc) << 16) | u64::from(sequence);
        let highest_index = self
            .last_sequence
            .map(|last| (u64::from(self.roc) << 16) | u64::from(last));
        if highest_index.is_none_or(|highest| index > highest) {
            self.roc = packet_roc;
            self.last_sequence = Some(sequence);
        }
        let mut out = payload.to_vec();
        let keystream = self.keystream(ssrc, index, payload.len());
        for (byte, key) in out.iter_mut().zip(keystream) {
            *byte ^= key;
        }
        Ok(out)
    }

    fn guess_roc(&self, sequence: u16) -> Result<u32> {
        let Some(last) = self.last_sequence else {
            return Ok(self.roc);
        };
        if last < 0x8000 && sequence > last && sequence - last > 0x8000 {
            return Ok(self.roc.saturating_sub(1));
        }
        if last >= 0x8000 && sequence < last && last - sequence > 0x8000 {
            return self
                .roc
                .checked_add(1)
                .ok_or(Error::LimitExceeded("SRTP rollover counter"));
        }
        Ok(self.roc)
    }

    fn keystream(&self, ssrc: u32, index: u64, len: usize) -> Vec<u8> {
        // RFC 3711 AES-CM counter block:
        //   IV = (salt << 16) XOR (SSRC << 64) XOR (packet_index << 16)
        //
        // The key-derivation labels are not part of the per-packet IV. The
        // final two bytes remain zero and are used as the AES block counter.
        let mut block = [0u8; 16];
        block[..SALT_LEN].copy_from_slice(&self.salt);
        xor_into(&mut block[8..12], &self.salt[8..12], &ssrc.to_be_bytes());
        let index_bytes = index.to_be_bytes();
        for (slot, byte) in block[8..14].iter_mut().zip(&index_bytes[2..]) {
            *slot ^= *byte;
        }

        let mut stream = Vec::with_capacity(len);
        let mut counter = block;
        while stream.len() < len {
            let mut out_block = GenericArray::default();
            self.cipher
                .encrypt_block_b2b(&GenericArray::from(counter), &mut out_block);
            stream.extend_from_slice(&out_block);
            increment_counter(&mut counter);
        }
        stream.truncate(len);
        stream
    }
}

fn xor_into(dst: &mut [u8], a: &[u8], b: &[u8]) {
    for (slot, (x, y)) in dst.iter_mut().zip(a.iter().zip(b.iter())) {
        *slot = x ^ y;
    }
}

fn increment_counter(counter: &mut [u8; 16]) {
    for byte in counter.iter_mut().rev() {
        *byte = byte.wrapping_add(1);
        if *byte != 0 {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_aes_cm_keystream() {
        let mut key = [0u8; MEDIA_STREAM_KEY_LEN];
        for (i, byte) in key.iter_mut().enumerate() {
            *byte = (i * 7 + 3) as u8;
        }
        let mut sender = SrtpContext::from_key_blob(&key).expect("context");
        let mut receiver = SrtpContext::from_key_blob(&key).expect("context");
        let plaintext = b"hello srtp payload 0123456789";
        let encrypted = sender
            .decrypt_rtp_payload(0xdead_beef, 42, plaintext)
            .expect("encrypt via keystream");
        assert_ne!(&encrypted, plaintext);
        let decrypted = receiver
            .decrypt_rtp_payload(0xdead_beef, 42, &encrypted)
            .expect("decrypt");
        assert_eq!(&decrypted, plaintext);
    }

    #[test]
    fn handles_sequence_wrap() {
        let key = [7u8; MEDIA_STREAM_KEY_LEN];
        let mut sender = SrtpContext::from_key_blob(&key).expect("context");
        let mut receiver = SrtpContext::from_key_blob(&key).expect("context");
        let payload = b"wrap";
        let first_cipher = sender
            .decrypt_rtp_payload(1, u16::MAX, payload)
            .expect("first encrypt");
        let first_plain = receiver
            .decrypt_rtp_payload(1, u16::MAX, &first_cipher)
            .expect("first decrypt");
        assert_eq!(&first_plain, payload);

        let second_cipher = sender
            .decrypt_rtp_payload(1, 0, payload)
            .expect("second encrypt");
        let second_plain = receiver
            .decrypt_rtp_payload(1, 0, &second_cipher)
            .expect("second decrypt");
        assert_eq!(&second_plain, payload);
        assert_eq!(receiver.roc, 1);
    }

    #[test]
    fn matches_rfc3711_aes_cm_vector() {
        let mut blob = [0u8; MEDIA_STREAM_KEY_LEN];
        blob[..KEY_LEN].copy_from_slice(&[
            0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6, 0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf,
            0x4f, 0x3c,
        ]);
        blob[KEY_LEN..KEY_LEN + SALT_LEN].copy_from_slice(&[
            0xf0, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa, 0xfb, 0xfc, 0xfd,
        ]);
        let mut context = SrtpContext::from_key_blob(&blob).expect("context");
        let keystream = context
            .decrypt_rtp_payload(0, 0, &[0u8; 16])
            .expect("keystream");
        assert_eq!(
            keystream,
            vec![
                0xe0, 0x3e, 0xad, 0x09, 0x35, 0xc9, 0x5e, 0x80, 0xe1, 0x66, 0xb1, 0x6d, 0xd9, 0x2b,
                0x4e, 0xb4,
            ]
        );
    }
}
