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
        if input.len() > self.max_message_bytes.saturating_sub(self.buffer.len()) {
            return Err(Error::LimitExceeded("ARD buffered messages"));
        }
        let previous_len = self.buffer.len();
        self.buffer.extend_from_slice(input);
        match self.first_message_len(decoder) {
            Err(Error::NeedMore { .. }) => return Ok(Vec::new()),
            Err(error) => {
                self.buffer.truncate(previous_len);
                return Err(error);
            }
            Ok(_) => {}
        }

        // Clone only once a complete message is present. Large framebuffer
        // updates commonly span many encrypted records; cloning on every
        // fragment made buffering quadratic in the frame size.
        let mut next = self.clone();
        self.buffer.truncate(previous_len);
        let messages = next.drain_available(decoder, framebuffer)?;
        *self = next;
        Ok(messages)
    }

    pub fn buffered_bytes(&self) -> usize {
        self.buffer.len()
    }

    fn drain_available(
        &mut self,
        decoder: &mut Decoder,
        framebuffer: &mut Framebuffer,
    ) -> Result<Vec<ArdServerMessage>> {
        let mut messages = Vec::new();
        loop {
            let Some(message_type) = self.buffer.first().copied() else {
                return Ok(messages);
            };
            match message_type {
                0 => {
                    if self.buffer.len() < 4 {
                        return Ok(messages);
                    }
                    let rectangle_count =
                        usize::from(u16::from_be_bytes([self.buffer[2], self.buffer[3]]));
                    let consumed =
                        match parse_complete_framebuffer_update(&self.buffer, decoder, framebuffer)
                        {
                            Ok(consumed) => consumed,
                            Err(Error::NeedMore { .. }) => return Ok(messages),
                            Err(error) => return Err(error),
                        };
                    self.buffer.drain(..consumed);
                    messages.push(ArdServerMessage::FramebufferUpdate {
                        rectangle_count,
                        bytes: consumed,
                    });
                    if let Some(control) = decoder.take_ard_encryption_control() {
                        messages.push(ArdServerMessage::EncryptionControl(control));
                    }
                }
                2 => {
                    self.buffer.remove(0);
                    messages.push(ArdServerMessage::Bell);
                }
                3 => {
                    let (consumed, text) = match self.read_cut_text() {
                        Ok(message) => message,
                        Err(Error::NeedMore { .. }) => return Ok(messages),
                        Err(error) => return Err(error),
                    };
                    self.buffer.drain(..consumed);
                    messages.push(ArdServerMessage::ServerCutText(text));
                }
                _ => return Err(Error::Invalid("unsupported ARD server message type")),
            }
        }
    }

    fn first_message_len(&self, decoder: &Decoder) -> Result<usize> {
        let Some(message_type) = self.buffer.first().copied() else {
            return Err(Error::NeedMore {
                needed: 1,
                available: 0,
            });
        };
        match message_type {
            0 => complete_framebuffer_update_len(&self.buffer, decoder),
            2 => Ok(1),
            3 => {
                if self.buffer.len() < 8 {
                    return Err(Error::NeedMore {
                        needed: 8,
                        available: self.buffer.len(),
                    });
                }
                let len = usize::try_from(u32::from_be_bytes(
                    self.buffer[4..8]
                        .try_into()
                        .expect("cut text length checked"),
                ))
                .map_err(|_| Error::LimitExceeded("ARD cut-text length"))?;
                if len > self.max_cut_text_bytes {
                    return Err(Error::LimitExceeded("ARD cut-text length"));
                }
                let total = len
                    .checked_add(8)
                    .ok_or(Error::LimitExceeded("ARD cut-text length"))?;
                if self.buffer.len() < total {
                    Err(Error::NeedMore {
                        needed: total,
                        available: self.buffer.len(),
                    })
                } else {
                    Ok(total)
                }
            }
            _ => Err(Error::Invalid("unsupported ARD server message type")),
        }
    }

    fn read_cut_text(&self) -> Result<(usize, String)> {
        let mut cursor = Cursor::new(&self.buffer);
        cursor.u8()?;
        cursor.take(3)?;
        let len = usize::try_from(cursor.u32()?)
            .map_err(|_| Error::LimitExceeded("ARD cut-text length"))?;
        if len > self.max_cut_text_bytes {
            return Err(Error::LimitExceeded("ARD cut-text length"));
        }
        let text = core::str::from_utf8(cursor.take(len)?)
            .map_err(|_| Error::Invalid("ARD cut-text is not UTF-8"))?
            .to_owned();
        Ok((8 + len, text))
    }
}
