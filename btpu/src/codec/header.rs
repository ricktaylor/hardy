use crate::codec::{Error, message::MessageFlags};

/// Size of the standard message header in bytes.
pub const HEADER_SIZE: usize = 4;

/// Maximum value of the 20-bit content-length field.
pub const MAX_CONTENT_LENGTH: usize = 0xF_FFFF; // 1,048,575

/// A decoded BTP-U message header.
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
    pub message_type: u8,
    pub flags: MessageFlags,
    pub length: u32,
}

/// Encode a message header into a 4-byte destination slice.
///
/// # Panics
///
/// Panics if `dst` is shorter than [`HEADER_SIZE`].
pub fn encode_header(header: &MessageHeader, dst: &mut [u8]) {
    debug_assert!(header.length as usize <= MAX_CONTENT_LENGTH);
    dst[0] = header.message_type;
    let flags_nibble = header.flags.to_nibble();
    dst[1] = (flags_nibble << 4) | ((header.length >> 16) as u8 & 0x0F);
    dst[2] = (header.length >> 8) as u8;
    dst[3] = header.length as u8;
}

/// Decode a message header from a byte slice.
///
/// Returns an error if `src` is shorter than [`HEADER_SIZE`].
pub fn decode_header(src: &[u8]) -> Result<MessageHeader, Error> {
    if src.len() < HEADER_SIZE {
        return Err(Error::InsufficientData {
            needed: HEADER_SIZE,
            available: src.len(),
        });
    }
    let message_type = src[0];
    let flags = MessageFlags::from_nibble(src[1] >> 4);
    let length = ((src[1] as u32 & 0x0F) << 16) | ((src[2] as u32) << 8) | (src[3] as u32);
    Ok(MessageHeader {
        message_type,
        flags,
        length,
    })
}
