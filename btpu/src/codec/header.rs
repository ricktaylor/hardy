//! The four-byte message header (Section 7).

use crate::codec::{Error, Result, message::MessageFlags};

/// Size of the standard message header in bytes.
pub const HEADER_SIZE: usize = 4;

/// Maximum value of the 20-bit content-length field.
///
/// Held as a `usize` because it bounds in-memory buffer sizes; see
/// [`ContentLength`] for the field's value type.
pub const MAX_CONTENT_LENGTH: usize = 0xF_FFFF; // 1,048,575

/// A message content length: a value of the header's 20-bit Length field
/// (Section 7), so at most [`MAX_CONTENT_LENGTH`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentLength(usize);

impl ContentLength {
    /// The largest content length the field can declare.
    pub const MAX: Self = Self(MAX_CONTENT_LENGTH);

    /// Returns the content length `len`, or `None` if it exceeds
    /// [`MAX_CONTENT_LENGTH`].
    pub const fn new(len: usize) -> Option<Self> {
        if len <= MAX_CONTENT_LENGTH {
            Some(Self(len))
        } else {
            None
        }
    }

    /// The length in bytes.
    pub const fn get(self) -> usize {
        self.0
    }
}

impl TryFrom<usize> for ContentLength {
    type Error = Error;

    /// Errors with [`Error::LengthOverflow`] if `len` exceeds
    /// [`MAX_CONTENT_LENGTH`].
    fn try_from(len: usize) -> Result<Self> {
        Self::new(len).ok_or(Error::LengthOverflow {
            length: len,
            max: MAX_CONTENT_LENGTH,
        })
    }
}

/// A decoded BTP-U message header (Section 7).
///
/// Layout (4 bytes, network byte order):
/// ```text
///  0                   1                   2                   3
///  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |     Type      | Flags |    Length (20-bit unsigned int)       |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageHeader {
    /// The message type value (Section 12.1 registry).
    pub message_type: u8,
    /// The four flag bits (Section 7.1).
    pub flags: MessageFlags,
    /// The content length in bytes.
    pub length: ContentLength,
}

/// Encode a message header into its 4-byte wire form.
pub fn encode_header(header: &MessageHeader) -> [u8; HEADER_SIZE] {
    // At most 20 bits by construction, so the low three bytes hold it.
    let [.., high, mid, low] = header.length.get().to_be_bytes();
    [
        header.message_type,
        (header.flags.to_nibble() << 4) | high,
        mid,
        low,
    ]
}

/// Decode a message header from a byte slice.
///
/// Errors with [`Error::InsufficientData`] if `src` is shorter than
/// [`HEADER_SIZE`].
pub fn decode_header(src: &[u8]) -> Result<MessageHeader> {
    let [message_type, flags_high, mid, low, ..] = *src else {
        return Err(Error::InsufficientData {
            needed: HEADER_SIZE,
            available: src.len(),
        });
    };
    let length = usize::from(flags_high & 0x0F) << 16 | usize::from(mid) << 8 | usize::from(low);
    Ok(MessageHeader {
        message_type,
        flags: MessageFlags::from_nibble(flags_high >> 4),
        length: ContentLength(length),
    })
}
