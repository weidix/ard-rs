use crate::protocol::{complete_framebuffer_update_len, parse_complete_framebuffer_update};
use crate::wire::Cursor;
use crate::{ArdEncryptionControl, Decoder, Error, Framebuffer, Result};

/// One complete server message recovered from the decrypted record payload
/// stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArdServerMessage {
    FramebufferUpdate {
        rectangle_count: usize,
        bytes: usize,
    },
    /// The zero-sized 1103 rectangle inside a FramebufferUpdate. The decoder
    /// stores it separately so the session material can be unwrapped.
    EncryptionControl(ArdEncryptionControl),
    Bell,
    ServerCutText(String),
    /// Native Screen Sharing's fixed-width user/session state notification.
    /// The viewer does not need to render this message.
    StateChange,
}

/// Bounded, incremental dispatcher for the internal RFB/ARD server-message
/// stream recovered after 1103 record decryption.
///
/// The native `_WaitForEncryptedMessage` appends every verified record payload
/// to the same net buffer that ordinary server messages are read from, so
/// this type accepts arbitrary payload fragments and parses complete
/// messages. FramebufferUpdate rectangles are routed through the existing
/// decoder, which keeps the persistent Apple zlib streams and the MVS state
/// machine across messages. The buffered stream is transactional (a malformed
/// message is not consumed); decoder and framebuffer mutation follows the
/// same contract as `parse_framebuffer_update`.
#[derive(Debug, Clone)]
pub struct ArdMessageDispatcher {
    buffer: Vec<u8>,
    read_offset: usize,
    max_message_bytes: usize,
    max_cut_text_bytes: usize,
}

impl ArdMessageDispatcher {
    pub fn new(max_message_bytes: usize, max_cut_text_bytes: usize) -> Result<Self> {
        if max_message_bytes < 4 {
            return Err(Error::Invalid("invalid ARD message size limit"));
        }
        Ok(Self {
            buffer: Vec::new(),
            read_offset: 0,
            max_message_bytes,
            max_cut_text_bytes,
        })
    }

    /// Feeds one decrypted record payload (or fragment) and returns every
    /// complete server message it contains. A malformed message is not
    /// consumed from the buffered stream.
    pub fn push(
        &mut self,
        input: &[u8],
        decoder: &mut Decoder,
        framebuffer: &mut Framebuffer,
    ) -> Result<Vec<ArdServerMessage>> {
        if input.len() > self.max_message_bytes.saturating_sub(self.buffered_bytes()) {
            return Err(Error::LimitExceeded("ARD buffered messages"));
        }
        let previous_len = self.buffer.len();
        self.buffer.extend_from_slice(input);
        let (consumed, messages) = match self.drain_available(decoder, framebuffer) {
            Ok(result) => result,
            Err(error) => {
                // `drain_available` parses against an offset and does not
                // mutate the buffer until the whole batch succeeds. Keeping
                // the input in place lets this rollback restore the exact
                // pre-push bytes without cloning a large framebuffer update.
                self.buffer.truncate(previous_len);
                return Err(error);
            }
        };
        self.read_offset = self.read_offset.saturating_add(consumed);
        self.compact_consumed();
        Ok(messages)
    }

    pub fn buffered_bytes(&self) -> usize {
        self.buffer.len().saturating_sub(self.read_offset)
    }

    fn compact_consumed(&mut self) {
        if self.read_offset == 0 {
            return;
        }
        if self.read_offset == self.buffer.len() {
            self.buffer.clear();
            self.read_offset = 0;
            return;
        }
        if self.read_offset < 4096 || self.read_offset < self.buffer.len() / 2 {
            return;
        }
        let remaining = self.buffer.len() - self.read_offset;
        self.buffer.copy_within(self.read_offset.., 0);
        self.buffer.truncate(remaining);
        self.read_offset = 0;
    }

    fn drain_available(
        &mut self,
        decoder: &mut Decoder,
        framebuffer: &mut Framebuffer,
    ) -> Result<(usize, Vec<ArdServerMessage>)> {
        let mut messages = Vec::new();
        let mut consumed = 0_usize;
        loop {
            let buffer_start = self.read_offset.saturating_add(consumed);
            let buffer = &self.buffer[buffer_start..];
            let Some(message_type) = buffer.first().copied() else {
                return Ok((consumed, messages));
            };
            match self.first_message_len(buffer, decoder) {
                Err(Error::NeedMore { .. }) => return Ok((consumed, messages)),
                Err(error) => return Err(error),
                Ok(_) => {}
            };
            match message_type {
                0 => {
                    let rectangle_count = usize::from(u16::from_be_bytes([buffer[2], buffer[3]]));
                    let message_len =
                        match parse_complete_framebuffer_update(buffer, decoder, framebuffer) {
                            Ok(consumed) => consumed,
                            Err(Error::NeedMore { .. }) => return Ok((consumed, messages)),
                            Err(error) => return Err(error),
                        };
                    consumed = consumed
                        .checked_add(message_len)
                        .ok_or(Error::LimitExceeded("ARD buffered messages"))?;
                    messages.push(ArdServerMessage::FramebufferUpdate {
                        rectangle_count,
                        bytes: message_len,
                    });
                    if let Some(control) = decoder.take_ard_encryption_control() {
                        messages.push(ArdServerMessage::EncryptionControl(control));
                    }
                }
                2 => {
                    consumed += 1;
                    messages.push(ArdServerMessage::Bell);
                }
                3 => {
                    let (message_len, text) =
                        match Self::read_cut_text(buffer, self.max_cut_text_bytes) {
                            Ok(message) => message,
                            Err(Error::NeedMore { .. }) => return Ok((consumed, messages)),
                            Err(error) => return Err(error),
                        };
                    consumed = consumed
                        .checked_add(message_len)
                        .ok_or(Error::LimitExceeded("ARD buffered messages"))?;
                    messages.push(ArdServerMessage::ServerCutText(text));
                }
                0x14 => {
                    consumed += 8;
                    messages.push(ArdServerMessage::StateChange);
                }
                other => return Err(Error::UnsupportedServerMessage(other)),
            }
        }
    }

    fn first_message_len(&self, buffer: &[u8], decoder: &Decoder) -> Result<usize> {
        let Some(message_type) = buffer.first().copied() else {
            return Err(Error::NeedMore {
                needed: 1,
                available: 0,
            });
        };
        match message_type {
            0 => complete_framebuffer_update_len(buffer, decoder),
            2 => Ok(1),
            3 => {
                if buffer.len() < 8 {
                    return Err(Error::NeedMore {
                        needed: 8,
                        available: buffer.len(),
                    });
                }
                let len = usize::try_from(u32::from_be_bytes(
                    buffer[4..8].try_into().expect("cut text length checked"),
                ))
                .map_err(|_| Error::LimitExceeded("ARD cut-text length"))?;
                if len > self.max_cut_text_bytes {
                    return Err(Error::LimitExceeded("ARD cut-text length"));
                }
                let total = len
                    .checked_add(8)
                    .ok_or(Error::LimitExceeded("ARD cut-text length"))?;
                if buffer.len() < total {
                    Err(Error::NeedMore {
                        needed: total,
                        available: buffer.len(),
                    })
                } else {
                    Ok(total)
                }
            }
            0x14 => {
                if buffer.len() < 8 {
                    Err(Error::NeedMore {
                        needed: 8,
                        available: buffer.len(),
                    })
                } else {
                    Ok(8)
                }
            }
            other => Err(Error::UnsupportedServerMessage(other)),
        }
    }

    fn read_cut_text(buffer: &[u8], max_cut_text_bytes: usize) -> Result<(usize, String)> {
        let mut cursor = Cursor::new(buffer);
        cursor.u8()?;
        cursor.take(3)?;
        let len = usize::try_from(cursor.u32()?)
            .map_err(|_| Error::LimitExceeded("ARD cut-text length"))?;
        if len > max_cut_text_bytes {
            return Err(Error::LimitExceeded("ARD cut-text length"));
        }
        let text = core::str::from_utf8(cursor.take(len)?)
            .map_err(|_| Error::Invalid("ARD cut-text is not UTF-8"))?
            .to_owned();
        Ok((8 + len, text))
    }
}
