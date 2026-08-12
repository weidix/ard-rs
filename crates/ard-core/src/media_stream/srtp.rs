//! SRTP decryption for Apple's real-time media stream.
//!
//! The AVC key blob contains a 32-byte master key followed by a 14-byte
//! master salt. Cipher suite 5 expands those values directly into a 32-byte
//! AES-256 session key and a 14-byte session salt. Payloads are encrypted
//! with AES-CTR using the native AVC IV layout.

use std::fmt;

use aes::Aes256;
use aes::cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};
use sha1::{Digest, Sha1};
use subtle::ConstantTimeEq;

use crate::{Error, Result};

use super::MEDIA_STREAM_KEY_LEN;

const MASTER_KEY_LEN: usize = 32;
const MASTER_SALT_LEN: usize = 14;
const SESSION_KEY_LEN: usize = 32;
const SESSION_AUTH_KEY_LEN: usize = 20;
const SESSION_SALT_LEN: usize = 14;
/// Cipher suite 5 appends a ten-byte HMAC-SHA1 authentication tag to each
/// RTP packet.
pub(crate) const AUTH_TAG_LEN: usize = 10;

// `_SRTPUseEncryptionInternal` selects the standard SRTP labels 0/2 for RTP
// and 3/5 for SRTCP. The selector is RTP-vs-RTCP, not send-vs-receive; both
// media directions therefore use the RTP pair 0/2.
const RTP_ENCRYPTION_LABEL: u8 = 0;
const RTP_AUTHENTICATION_LABEL: u8 = 1;
const RTP_SALT_LABEL: u8 = 2;
const SRTCP_ENCRYPTION_LABEL: u8 = 3;
const SRTCP_AUTHENTICATION_LABEL: u8 = 4;
const SRTCP_SALT_LABEL: u8 = 5;

#[derive(Clone)]
struct SessionMaterial {
    cipher: Aes256,
    authentication_key: [u8; SESSION_AUTH_KEY_LEN],
    salt: [u8; SESSION_SALT_LEN],
}

/// SRTP decryption context for one AVC media stream.
#[derive(Clone)]
pub struct SrtpContext {
    material: SessionMaterial,
    /// The negotiated server SSRC from the answer. The live receiver filters
    /// incoming packets to this SSRC before using the context.
    derived_ssrc: u32,
    /// Rollover counter for the highest packet observed so far.
    roc: u32,
    last_sequence: Option<u16>,
    /// Highest packet index and the 64-packet replay window.
    highest_index: Option<u64>,
    replay_window: u64,
}

/// SRTCP sender context for the receiver reports expected by Apple's AVC
/// server. Unlike RTP, the 31-bit SRTCP packet index is carried on the wire.
pub struct SrtcpContext {
    material: SessionMaterial,
    sender_ssrc: u32,
    index: u32,
}

impl fmt::Debug for SrtcpContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SrtcpContext")
            .field("sender_ssrc", &self.sender_ssrc)
            .field("index", &self.index)
            .finish()
    }
}

impl SrtcpContext {
    /// Build the outbound feedback context from the viewer-to-server key.
    pub fn from_key_blob_with_sender_ssrc(blob: &[u8], sender_ssrc: u32) -> Result<Self> {
        if blob.len() != MEDIA_STREAM_KEY_LEN {
            return Err(Error::Invalid("SRTCP key blob must be 46 bytes"));
        }
        let mut master_key = [0u8; MASTER_KEY_LEN];
        let mut master_salt = [0u8; MASTER_SALT_LEN];
        master_key.copy_from_slice(&blob[..MASTER_KEY_LEN]);
        master_salt.copy_from_slice(&blob[MASTER_KEY_LEN..]);
        let material = derive_session_material_with_labels(
            &master_key,
            &master_salt,
            SRTCP_ENCRYPTION_LABEL,
            SRTCP_AUTHENTICATION_LABEL,
            SRTCP_SALT_LABEL,
        );
        master_key.fill(0);
        master_salt.fill(0);
        Ok(Self {
            material,
            sender_ssrc,
            index: 0,
        })
    }

    /// Create the encrypted/authenticated 22-byte AVC receiver keep-alive.
    /// Packet type 192 and the empty RTCP payload match the native client;
    /// the index and authentication tag are unique to this negotiated session.
    pub fn protect_heartbeat(&mut self) -> Result<Vec<u8>> {
        let mut packet = Vec::with_capacity(8);
        packet.extend_from_slice(&[0x80, 0xc0, 0x00, 0x01]);
        packet.extend_from_slice(&self.sender_ssrc.to_be_bytes());
        self.protect_rtcp(packet)
    }

    /// Create the native 26-byte receiver report for one simulcast SSRC.
    /// The remote SSRC is the four-byte encrypted RTCP payload.
    pub fn protect_receiver_report(&mut self, remote_ssrc: u32) -> Result<Vec<u8>> {
        let mut packet = Vec::with_capacity(12);
        packet.extend_from_slice(&[0x80, 0xc0, 0x00, 0x02]);
        packet.extend_from_slice(&self.sender_ssrc.to_be_bytes());
        packet.extend_from_slice(&remote_ssrc.to_be_bytes());
        self.protect_rtcp(packet)
    }

    /// Create an RFC 4585 Picture Loss Indication for one remote video SSRC.
    /// The common sender/media SSRC fields are protected as the SRTCP payload,
    /// matching the encryption/authentication rules used by the native stream.
    pub fn protect_picture_loss_indication(&mut self, remote_ssrc: u32) -> Result<Vec<u8>> {
        let mut packet = Vec::with_capacity(12);
        // V=2, FMT=1 (PLI), PT=206 (payload-specific feedback), length=2.
        packet.extend_from_slice(&[0x81, 0xce, 0x00, 0x02]);
        packet.extend_from_slice(&self.sender_ssrc.to_be_bytes());
        packet.extend_from_slice(&remote_ssrc.to_be_bytes());
        self.protect_rtcp(packet)
    }

    fn protect_rtcp(&mut self, mut packet: Vec<u8>) -> Result<Vec<u8>> {
        let index = self
            .index
            .checked_add(1)
            .filter(|index| *index <= 0x7fff_fffe)
            .ok_or(Error::LimitExceeded("SRTCP packet index"))?;
        if packet.len() < 8 {
            return Err(Error::Invalid("SRTCP packet is too short"));
        }
        let keystream = self.keystream(index, packet.len() - 8);
        for (byte, key) in packet[8..].iter_mut().zip(keystream) {
            *byte ^= key;
        }
        packet.extend_from_slice(&(index | 0x8000_0000).to_be_bytes());
        let digest = hmac_sha1(&self.material.authentication_key, &packet);
        packet.extend_from_slice(&digest[..AUTH_TAG_LEN]);
        self.index = index;
        Ok(packet)
    }

    fn keystream(&self, index: u32, len: usize) -> Vec<u8> {
        let mut block = [0u8; 16];
        block[..SESSION_SALT_LEN].copy_from_slice(&self.material.salt);
        xor_bytes(&mut block[4..8], &self.sender_ssrc.to_be_bytes());
        xor_bytes(&mut block[8..12], &(index >> 16).to_be_bytes());
        xor_bytes(&mut block[12..14], &(index as u16).to_be_bytes());

        let mut stream = Vec::with_capacity(len);
        let mut counter = block;
        while stream.len() < len {
            let mut output = GenericArray::clone_from_slice(&counter);
            self.material.cipher.encrypt_block(&mut output);
            stream.extend_from_slice(&output);
            increment_counter(&mut counter);
        }
        stream.truncate(len);
        stream
    }
}

impl fmt::Debug for SrtpContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SrtpContext")
            .field("derived_ssrc", &self.derived_ssrc)
            .field("roc", &self.roc)
            .field("last_sequence", &self.last_sequence)
            .field("highest_index", &self.highest_index)
            .field("replay_window", &self.replay_window)
            .finish()
    }
}

impl SrtpContext {
    /// Build a context from a 46-byte negotiated key blob.
    ///
    /// This constructor is retained for callers that only need the crypto
    /// primitive. The live AVC path must use
    /// [`Self::from_key_blob_with_derived_ssrc`] so the native context is
    /// explicit.
    pub fn from_key_blob(blob: &[u8]) -> Result<Self> {
        Self::from_key_blob_with_derived_ssrc(blob, 0)
    }

    /// Build a context with the negotiated server SSRC.
    pub fn from_key_blob_with_derived_ssrc(blob: &[u8], derived_ssrc: u32) -> Result<Self> {
        if blob.len() != MEDIA_STREAM_KEY_LEN {
            return Err(Error::Invalid("SRTP key blob must be 46 bytes"));
        }
        let mut master_key = [0u8; MASTER_KEY_LEN];
        let mut master_salt = [0u8; MASTER_SALT_LEN];
        master_key.copy_from_slice(&blob[..MASTER_KEY_LEN]);
        master_salt.copy_from_slice(&blob[MASTER_KEY_LEN..]);
        let material = derive_session_material(&master_key, &master_salt);
        master_key.fill(0);
        master_salt.fill(0);
        Ok(Self {
            material,
            derived_ssrc,
            roc: 0,
            last_sequence: None,
            highest_index: None,
            replay_window: 0,
        })
    }

    /// Reset packet-index and replay state while keeping the negotiated
    /// encryption material.
    pub fn reset(&mut self) {
        self.roc = 0;
        self.last_sequence = None;
        self.highest_index = None;
        self.replay_window = 0;
    }

    /// Decrypt an RTP packet payload.
    ///
    /// `sequence` comes from the cleartext RTP header. The SSRC used for the
    /// counter block is supplied at construction.
    pub fn decrypt_rtp_payload(&mut self, sequence: u16, payload: &[u8]) -> Result<Vec<u8>> {
        if payload.is_empty() {
            return Ok(Vec::new());
        }

        let packet_roc = self.guess_roc(sequence)?;
        let index = (u64::from(packet_roc) << 16) | u64::from(sequence);
        self.accept_replay_state(index, packet_roc, sequence)?;

        let mut out = payload.to_vec();
        let keystream = self.keystream(packet_roc, sequence, payload.len());
        for (byte, key) in out.iter_mut().zip(keystream) {
            *byte ^= key;
        }
        Ok(out)
    }

    /// Decrypt an RTP packet body without authenticating it. `payload_offset`
    /// points at the encrypted payload and the range to the end includes
    /// encrypted RTP padding, if present.
    pub fn decrypt_rtp_packet_in_place(
        &mut self,
        packet: &mut [u8],
        sequence: u16,
        payload_offset: usize,
    ) -> Result<()> {
        let packet_roc = self.guess_roc(sequence)?;
        self.decrypt_rtp_packet_in_place_with_roc(packet, packet_roc, sequence, payload_offset)
    }

    /// Authenticate and decrypt one cipher-suite-5 RTP body. `packet` must
    /// exclude the ten-byte authentication tag, while `authentication_tag`
    /// contains exactly that removed suffix.
    pub fn decrypt_authenticated_rtp_packet_in_place(
        &mut self,
        packet: &mut [u8],
        authentication_tag: &[u8],
        sequence: u16,
        payload_offset: usize,
    ) -> Result<()> {
        if authentication_tag.len() != AUTH_TAG_LEN {
            return Err(Error::Invalid("SRTP authentication tag length"));
        }
        let packet_roc = self.guess_roc(sequence)?;
        let expected = self.authentication_tag(packet, packet_roc);
        if expected.as_slice().ct_eq(authentication_tag).unwrap_u8() == 0 {
            return Err(Error::Invalid("SRTP authentication failed"));
        }
        self.decrypt_rtp_packet_in_place_with_roc(packet, packet_roc, sequence, payload_offset)
    }

    /// Decrypt an RTP packet using an explicit rollover counter.
    pub fn decrypt_rtp_packet_in_place_with_roc(
        &mut self,
        packet: &mut [u8],
        packet_roc: u32,
        sequence: u16,
        payload_offset: usize,
    ) -> Result<()> {
        if payload_offset > packet.len() {
            return Err(Error::Invalid("RTP payload offset"));
        }

        let index = (u64::from(packet_roc) << 16) | u64::from(sequence);
        self.accept_replay_state(index, packet_roc, sequence)?;
        let keystream = self.keystream(packet_roc, sequence, packet.len() - payload_offset);
        for (byte, key) in packet[payload_offset..].iter_mut().zip(keystream) {
            *byte ^= key;
        }
        Ok(())
    }

    fn guess_roc(&self, sequence: u16) -> Result<u32> {
        let Some(highest_index) = self.highest_index else {
            return Ok(self.roc);
        };
        let last = highest_index as u16;
        let roc = (highest_index >> 16) as u32;
        if last < 0x8000 && sequence > last && sequence - last > 0x8000 {
            return Ok(roc.saturating_sub(1));
        }
        if last >= 0x8000 && sequence < last && last - sequence > 0x8000 {
            return roc
                .checked_add(1)
                .ok_or(Error::LimitExceeded("SRTP rollover counter"));
        }
        Ok(roc)
    }

    fn accept_replay_state(&mut self, index: u64, packet_roc: u32, sequence: u16) -> Result<()> {
        let Some(highest) = self.highest_index else {
            self.highest_index = Some(index);
            self.replay_window = 1;
            self.roc = packet_roc;
            self.last_sequence = Some(sequence);
            return Ok(());
        };

        if index > highest {
            let shift = index - highest;
            self.replay_window = if shift >= 64 {
                1
            } else {
                (self.replay_window << shift) | 1
            };
            self.highest_index = Some(index);
            self.roc = packet_roc;
            self.last_sequence = Some(sequence);
        } else {
            let distance = highest - index;
            if distance < 64 {
                let bit = 1u64 << distance;
                if self.replay_window & bit != 0 {
                    return Err(Error::Invalid("SRTP replay detected"));
                }
                self.replay_window |= bit;
            } else {
                return Err(Error::Invalid("SRTP packet is outside replay window"));
            }
        }
        Ok(())
    }

    fn keystream(&self, packet_roc: u32, sequence: u16, len: usize) -> Vec<u8> {
        // Native AVC builds the counter block as:
        //
        //   session_salt[0..14] || 0x0000
        //   XOR derivedSSRC at [4..8]
        //   XOR ROC        at [8..12]
        //   XOR sequence   at [12..14]
        //
        // The context and sequence fields use their native network byte
        // layout. The CTR block is incremented in big-endian order by
        // CommonCrypto.
        let mut block = [0u8; 16];
        block[..SESSION_SALT_LEN].copy_from_slice(&self.material.salt);
        xor_bytes(&mut block[4..8], &self.derived_ssrc.to_be_bytes());
        xor_bytes(&mut block[8..12], &packet_roc.to_be_bytes());
        xor_bytes(&mut block[12..14], &sequence.to_be_bytes());

        let mut stream = Vec::with_capacity(len);
        let mut counter = block;
        while stream.len() < len {
            let mut output = GenericArray::clone_from_slice(&counter);
            self.material.cipher.encrypt_block(&mut output);
            stream.extend_from_slice(&output);
            increment_counter(&mut counter);
        }
        stream.truncate(len);
        stream
    }

    fn authentication_tag(&self, encrypted_packet: &[u8], packet_roc: u32) -> [u8; AUTH_TAG_LEN] {
        let mut authenticated = Vec::with_capacity(encrypted_packet.len() + 4);
        authenticated.extend_from_slice(encrypted_packet);
        authenticated.extend_from_slice(&packet_roc.to_be_bytes());
        let digest = hmac_sha1(&self.material.authentication_key, &authenticated);
        digest[..AUTH_TAG_LEN]
            .try_into()
            .expect("fixed authentication tag length")
    }
}

fn xor_bytes(destination: &mut [u8], source: &[u8]) {
    debug_assert_eq!(destination.len(), source.len());
    for (destination, source) in destination.iter_mut().zip(source) {
        *destination ^= *source;
    }
}

fn derive_session_material(
    master_key: &[u8; MASTER_KEY_LEN],
    master_salt: &[u8; MASTER_SALT_LEN],
) -> SessionMaterial {
    derive_session_material_with_labels(
        master_key,
        master_salt,
        RTP_ENCRYPTION_LABEL,
        RTP_AUTHENTICATION_LABEL,
        RTP_SALT_LABEL,
    )
}

fn derive_session_material_with_labels(
    master_key: &[u8; MASTER_KEY_LEN],
    master_salt: &[u8; MASTER_SALT_LEN],
    encryption_label: u8,
    authentication_label: u8,
    salt_label: u8,
) -> SessionMaterial {
    let mut encryption_key =
        aes_ecb_expand(master_key, master_salt, encryption_label, SESSION_KEY_LEN);
    let authentication_key = aes_ecb_expand(
        master_key,
        master_salt,
        authentication_label,
        SESSION_AUTH_KEY_LEN,
    );
    let salt = aes_ecb_expand(master_key, master_salt, salt_label, SESSION_SALT_LEN);
    let material = SessionMaterial {
        cipher: Aes256::new(GenericArray::from_slice(&encryption_key)),
        authentication_key: authentication_key
            .try_into()
            .expect("session authentication key length"),
        salt: salt.try_into().expect("session salt length"),
    };
    encryption_key.fill(0);
    material
}

fn hmac_sha1(key: &[u8], input: &[u8]) -> [u8; 20] {
    let mut key_block = [0u8; 64];
    if key.len() > key_block.len() {
        key_block[..20].copy_from_slice(&Sha1::digest(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = key_block;
    let mut outer_pad = key_block;
    for byte in &mut inner_pad {
        *byte ^= 0x36;
    }
    for byte in &mut outer_pad {
        *byte ^= 0x5c;
    }
    let mut inner = Sha1::new();
    inner.update(inner_pad);
    inner.update(input);
    let inner_digest = inner.finalize();
    let mut outer = Sha1::new();
    outer.update(outer_pad);
    outer.update(inner_digest);
    outer.finalize().into()
}

/// Reproduce `_MakeSessionKey`: every output block is an independent AES-ECB
/// encryption of `salt || 0x00 || counter`, with the label XORed into salt
/// byte 7.
fn aes_ecb_expand(
    key: &[u8],
    salt: &[u8; MASTER_SALT_LEN],
    label: u8,
    output_len: usize,
) -> Vec<u8> {
    debug_assert_eq!(key.len(), MASTER_KEY_LEN);
    let block_count = output_len.div_ceil(16);
    let cipher = Aes256::new(GenericArray::from_slice(key));
    let mut output = Vec::with_capacity(block_count * 16);
    for counter in 0..block_count {
        let mut input = [0u8; 16];
        input[..MASTER_SALT_LEN].copy_from_slice(salt);
        input[7] ^= label;
        input[15] = counter as u8;
        let mut block = GenericArray::clone_from_slice(&input);
        cipher.encrypt_block(&mut block);
        output.extend_from_slice(&block);
    }
    output.truncate(output_len);
    output
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
    fn session_material_matches_native_aes256_vector() {
        let mut master_key = [0u8; MASTER_KEY_LEN];
        let mut master_salt = [0u8; MASTER_SALT_LEN];
        for (index, byte) in master_key.iter_mut().enumerate() {
            *byte = index as u8;
        }
        for (index, byte) in master_salt.iter_mut().enumerate() {
            *byte = (index + MASTER_KEY_LEN) as u8;
        }
        assert_eq!(
            aes_ecb_expand(
                &master_key,
                &master_salt,
                RTP_ENCRYPTION_LABEL,
                SESSION_KEY_LEN
            ),
            [
                0x0e, 0x67, 0x4e, 0x2d, 0xb9, 0x0f, 0xd0, 0x74, 0xb7, 0xd1, 0x17, 0xff, 0x27, 0x48,
                0x25, 0x69, 0xaa, 0x77, 0x04, 0xa1, 0x2a, 0x1a, 0x9b, 0xda, 0x69, 0xd3, 0x3f, 0xcc,
                0x7f, 0x96, 0xdb, 0x40,
            ]
        );
        let material = derive_session_material(&master_key, &master_salt);
        assert_eq!(
            material.authentication_key,
            [
                0xdb, 0x98, 0xd1, 0xde, 0xdd, 0xa0, 0xab, 0x3e, 0x59, 0xc2, 0x04, 0x8a, 0x81, 0x3e,
                0x92, 0xd6, 0x06, 0x1c, 0x78, 0xc2,
            ]
        );
        assert_eq!(
            material.salt,
            [
                0x2b, 0x6d, 0xf1, 0x0e, 0x13, 0x7f, 0x60, 0x04, 0x52, 0x6e, 0xda, 0x84, 0x95, 0xd9,
            ]
        );
    }

    #[test]
    fn round_trips_apple_srtp_keystream() {
        let mut blob = [0u8; MEDIA_STREAM_KEY_LEN];
        for (i, byte) in blob.iter_mut().enumerate() {
            *byte = (i * 7 + 3) as u8;
        }
        let context = 0xdead_beef;
        let mut sender =
            SrtpContext::from_key_blob_with_derived_ssrc(&blob, context).expect("context");
        let mut receiver =
            SrtpContext::from_key_blob_with_derived_ssrc(&blob, context).expect("context");
        let plaintext = b"hello srtp payload 0123456789";
        let encrypted = sender
            .decrypt_rtp_payload(42, plaintext)
            .expect("encrypt via keystream");
        assert_ne!(&encrypted, plaintext);
        let decrypted = receiver
            .decrypt_rtp_payload(42, &encrypted)
            .expect("decrypt");
        assert_eq!(&decrypted, plaintext);
    }

    #[test]
    fn derived_ssrc_is_not_taken_from_packet_header() {
        let blob = [7u8; MEDIA_STREAM_KEY_LEN];
        let mut first = SrtpContext::from_key_blob_with_derived_ssrc(&blob, 1).expect("context");
        let mut second = SrtpContext::from_key_blob_with_derived_ssrc(&blob, 2).expect("context");
        let payload = b"same packet context";
        let first_cipher = first.decrypt_rtp_payload(1, payload).expect("encrypt");
        let second_cipher = second.decrypt_rtp_payload(1, payload).expect("encrypt");
        assert_ne!(first_cipher, second_cipher);
    }

    #[test]
    fn handles_sequence_wrap() {
        let blob = [7u8; MEDIA_STREAM_KEY_LEN];
        let context = 0x0102_0304;
        let mut sender =
            SrtpContext::from_key_blob_with_derived_ssrc(&blob, context).expect("context");
        let mut receiver =
            SrtpContext::from_key_blob_with_derived_ssrc(&blob, context).expect("context");
        let payload = b"wrap";
        let first_cipher = sender
            .decrypt_rtp_payload(u16::MAX, payload)
            .expect("first encrypt");
        let first_plain = receiver
            .decrypt_rtp_payload(u16::MAX, &first_cipher)
            .expect("first decrypt");
        assert_eq!(&first_plain, payload);

        let second_cipher = sender
            .decrypt_rtp_payload(0, payload)
            .expect("second encrypt");
        let second_plain = receiver
            .decrypt_rtp_payload(0, &second_cipher)
            .expect("second decrypt");
        assert_eq!(&second_plain, payload);
        assert_eq!(receiver.roc, 1);
    }

    #[test]
    fn accepts_reordered_packet_from_previous_roc() {
        let blob = [7u8; MEDIA_STREAM_KEY_LEN];
        let context = 0x0102_0304;
        let mut sender =
            SrtpContext::from_key_blob_with_derived_ssrc(&blob, context).expect("sender");
        let mut receiver =
            SrtpContext::from_key_blob_with_derived_ssrc(&blob, context).expect("receiver");
        let before_wrap = sender
            .decrypt_rtp_payload(u16::MAX - 1, b"before")
            .expect("encrypt before wrap");
        let after_wrap = sender
            .decrypt_rtp_payload(1, b"after")
            .expect("encrypt after wrap");
        assert_eq!(
            receiver
                .decrypt_rtp_payload(u16::MAX - 1, &before_wrap)
                .expect("decrypt before wrap"),
            b"before"
        );
        assert_eq!(
            receiver
                .decrypt_rtp_payload(1, &after_wrap)
                .expect("decrypt after wrap"),
            b"after"
        );

        let late_before_wrap = sender
            .decrypt_rtp_payload(u16::MAX, b"late")
            .expect("encrypt late packet");
        assert_eq!(
            receiver
                .decrypt_rtp_payload(u16::MAX, &late_before_wrap)
                .expect("decrypt late packet"),
            b"late"
        );
        assert_eq!(receiver.roc, 1);
    }

    #[test]
    fn tracks_reordering_without_changing_roc() {
        let blob = [7u8; MEDIA_STREAM_KEY_LEN];
        let context = 0x0102_0304;
        let mut receiver =
            SrtpContext::from_key_blob_with_derived_ssrc(&blob, context).expect("context");
        let mut sender =
            SrtpContext::from_key_blob_with_derived_ssrc(&blob, context).expect("context");
        let cipher_10 = sender.decrypt_rtp_payload(10, b"ten").expect("encrypt");
        let cipher_12 = sender.decrypt_rtp_payload(12, b"twelve").expect("encrypt");
        let cipher_11 = sender.decrypt_rtp_payload(11, b"eleven").expect("encrypt");
        assert_eq!(
            receiver.decrypt_rtp_payload(10, &cipher_10).unwrap(),
            b"ten"
        );
        assert_eq!(
            receiver.decrypt_rtp_payload(12, &cipher_12).unwrap(),
            b"twelve"
        );
        assert_eq!(
            receiver.decrypt_rtp_payload(11, &cipher_11).unwrap(),
            b"eleven"
        );
        assert_eq!(receiver.roc, 0);
        assert_eq!(receiver.last_sequence, Some(12));
    }

    #[test]
    fn decrypts_packet_body_and_rejects_replay() {
        let blob = [0x31u8; MEDIA_STREAM_KEY_LEN];
        let context = 0x1020_3040;
        let mut sender =
            SrtpContext::from_key_blob_with_derived_ssrc(&blob, context).expect("sender");
        let mut receiver =
            SrtpContext::from_key_blob_with_derived_ssrc(&blob, context).expect("receiver");

        let mut packet = vec![0x80, 100, 0, 7, 0, 0, 0, 1, 0, 0, 0, 2];
        let encrypted = sender
            .decrypt_rtp_payload(7, b"authenticated payload")
            .expect("encrypt");
        packet.extend_from_slice(&encrypted);

        receiver
            .decrypt_rtp_packet_in_place(&mut packet, 7, 12)
            .expect("decrypt");
        assert_eq!(&packet[12..], b"authenticated payload");
        assert!(
            receiver
                .decrypt_rtp_packet_in_place(&mut packet.clone(), 7, 12)
                .is_err()
        );
    }

    #[test]
    fn authenticates_before_decrypting_packet_body() {
        let blob = [0x42u8; MEDIA_STREAM_KEY_LEN];
        let ssrc = 0x0102_0304;
        let sequence = 7;
        let sender = SrtpContext::from_key_blob_with_derived_ssrc(&blob, ssrc).expect("sender");
        let mut packet = vec![0x80, 100, 0, sequence as u8, 0, 0, 0, 1, 1, 2, 3, 4];
        packet.extend_from_slice(b"authenticated payload");
        let keystream = sender.keystream(0, sequence, packet.len() - 12);
        for (byte, key) in packet[12..].iter_mut().zip(keystream) {
            *byte ^= key;
        }
        let tag = sender.authentication_tag(&packet, 0);
        let encrypted_packet = packet.clone();

        let mut receiver =
            SrtpContext::from_key_blob_with_derived_ssrc(&blob, ssrc).expect("receiver");
        receiver
            .decrypt_authenticated_rtp_packet_in_place(&mut packet, &tag, sequence, 12)
            .expect("authenticated decrypt");
        assert_eq!(&packet[12..], b"authenticated payload");

        let mut invalid_tag = tag;
        invalid_tag[0] ^= 1;
        let mut invalid_packet = encrypted_packet;
        let mut receiver =
            SrtpContext::from_key_blob_with_derived_ssrc(&blob, ssrc).expect("receiver");
        assert!(
            receiver
                .decrypt_authenticated_rtp_packet_in_place(
                    &mut invalid_packet,
                    &invalid_tag,
                    sequence,
                    12,
                )
                .is_err()
        );
        assert_eq!(receiver.highest_index, None);
    }

    #[test]
    fn creates_session_specific_srtcp_heartbeats() {
        let mut blob = [0u8; MEDIA_STREAM_KEY_LEN];
        for (index, byte) in blob.iter_mut().enumerate() {
            *byte = (index * 5 + 11) as u8;
        }
        let sender_ssrc = 0xb5ff_003e;
        let mut context =
            SrtcpContext::from_key_blob_with_sender_ssrc(&blob, sender_ssrc).expect("context");

        let first = context.protect_heartbeat().expect("first heartbeat");
        let second = context.protect_heartbeat().expect("second heartbeat");
        assert_eq!(first.len(), 22);
        assert_eq!(&first[..8], &[0x80, 0xc0, 0, 1, 0xb5, 0xff, 0, 0x3e]);
        assert_eq!(&first[8..12], &0x8000_0001u32.to_be_bytes());
        assert_eq!(&second[8..12], &0x8000_0002u32.to_be_bytes());
        assert_ne!(&first[12..], &second[12..]);

        let expected = hmac_sha1(&context.material.authentication_key, &first[..12]);
        assert_eq!(&first[12..], &expected[..AUTH_TAG_LEN]);
    }

    #[test]
    fn encrypts_native_srtcp_receiver_report_payload() {
        let blob = [0x5au8; MEDIA_STREAM_KEY_LEN];
        let local_ssrc = 0xb5ff_003e;
        let remote_ssrc = 0x6405_c090;
        let mut context =
            SrtcpContext::from_key_blob_with_sender_ssrc(&blob, local_ssrc).expect("context");
        let packet = context
            .protect_receiver_report(remote_ssrc)
            .expect("receiver report");

        assert_eq!(packet.len(), 26);
        assert_eq!(&packet[..8], &[0x80, 0xc0, 0, 2, 0xb5, 0xff, 0, 0x3e]);
        assert_ne!(&packet[8..12], &remote_ssrc.to_be_bytes());
        let keystream = context.keystream(1, 4);
        let decrypted: Vec<_> = packet[8..12]
            .iter()
            .zip(keystream)
            .map(|(byte, key)| byte ^ key)
            .collect();
        assert_eq!(decrypted, remote_ssrc.to_be_bytes());
        assert_eq!(&packet[12..16], &0x8000_0001u32.to_be_bytes());
        let expected = hmac_sha1(&context.material.authentication_key, &packet[..16]);
        assert_eq!(&packet[16..], &expected[..AUTH_TAG_LEN]);
    }

    #[test]
    fn protects_picture_loss_indication_for_the_remote_ssrc() {
        let blob = [0x3cu8; MEDIA_STREAM_KEY_LEN];
        let local_ssrc = 0x1020_3040;
        let remote_ssrc = 0x5060_7080;
        let mut context =
            SrtcpContext::from_key_blob_with_sender_ssrc(&blob, local_ssrc).expect("context");
        let packet = context
            .protect_picture_loss_indication(remote_ssrc)
            .expect("PLI");

        assert_eq!(
            &packet[..8],
            &[0x81, 0xce, 0x00, 0x02, 0x10, 0x20, 0x30, 0x40]
        );
        let keystream = context.keystream(1, 4);
        let decrypted: Vec<_> = packet[8..12]
            .iter()
            .zip(keystream)
            .map(|(byte, key)| byte ^ key)
            .collect();
        assert_eq!(decrypted, remote_ssrc.to_be_bytes());
        assert_eq!(&packet[12..16], &0x8000_0001u32.to_be_bytes());
    }
}
