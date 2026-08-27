use crate::codec::{Error, Result, message::MessageFlags};

/// Size of the standard message header in bytes.
pub const HEADER_SIZE: usize = 4;

/// Maximum value of the 20-bit content-length field.
///
/// Held as a `usize` because it bounds in-memory buffer sizes; the wire
/// field itself is decoded into [`MessageHeader::length`] as a `u32`, and a
/// 20-bit value converts to `usize` losslessly on every supported target.
pub const MAX_CONTENT_LENGTH: usize = 0xF_FFFF; // 1,048,575

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
    /// The content length in bytes, at most [`MAX_CONTENT_LENGTH`].
    pub length: u32,
}

/// Encode a message header into its 4-byte wire form.
///
/// Errors with [`Error::LengthOverflow`] if `header.length` exceeds
/// [`MAX_CONTENT_LENGTH`]; the 20-bit field cannot represent it, and
/// truncating would emit a header claiming a different length.
pub fn encode_header(header: &MessageHeader) -> Result<[u8; HEADER_SIZE]> {
    if header.length as usize > MAX_CONTENT_LENGTH {
        return Err(Error::LengthOverflow {
            length: header.length as usize,
            max: MAX_CONTENT_LENGTH,
        });
    }
    let flags_nibble = header.flags.to_nibble();
    let [_, high, mid, low] = header.length.to_be_bytes();
    Ok([
        header.message_type,
        (flags_nibble << 4) | (high & 0x0F),
        mid,
        low,
    ])
}

/// Decode a message header from a byte slice.
///
/// Errors with [`Error::InsufficientData`] if `src` is shorter than
/// [`HEADER_SIZE`].
pub fn decode_header(src: &[u8]) -> Result<MessageHeader> {
    if src.len() < HEADER_SIZE {
        return Err(Error::InsufficientData {
            needed: HEADER_SIZE,
            available: src.len(),
        });
    }
    let message_type = src[0];
    let flags = MessageFlags::from_nibble(src[1] >> 4);
    let length = u32::from_be_bytes([0, src[1] & 0x0F, src[2], src[3]]);
    Ok(MessageHeader {
        message_type,
        flags,
        length,
    })
}
