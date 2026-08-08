use crate::codec::Error;
use crate::message::MessageFlags;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_basic() {
        let hdr = MessageHeader {
            message_type: 3,
            flags: MessageFlags { hint: false },
            length: 256,
        };
        let mut buf = [0u8; 4];
        encode_header(&hdr, &mut buf);
        let decoded = decode_header(&buf).unwrap();
        assert_eq!(decoded, hdr);
    }

    #[test]
    fn round_trip_with_hint_flag() {
        let hdr = MessageHeader {
            message_type: 2,
            flags: MessageFlags { hint: true },
            length: 42,
        };
        let mut buf = [0u8; 4];
        encode_header(&hdr, &mut buf);
        let decoded = decode_header(&buf).unwrap();
        assert_eq!(decoded, hdr);
    }

    #[test]
    fn round_trip_max_length() {
        let hdr = MessageHeader {
            message_type: 1,
            flags: MessageFlags { hint: false },
            length: MAX_CONTENT_LENGTH as u32,
        };
        let mut buf = [0u8; 4];
        encode_header(&hdr, &mut buf);
        let decoded = decode_header(&buf).unwrap();
        assert_eq!(decoded, hdr);
    }

    #[test]
    fn round_trip_zero_length() {
        let hdr = MessageHeader {
            message_type: 5,
            flags: MessageFlags { hint: false },
            length: 0,
        };
        let mut buf = [0u8; 4];
        encode_header(&hdr, &mut buf);
        let decoded = decode_header(&buf).unwrap();
        assert_eq!(decoded, hdr);
    }

    #[test]
    fn decode_insufficient_data() {
        assert!(decode_header(&[0, 0]).is_err());
        assert!(decode_header(&[]).is_err());
    }

    #[test]
    fn wire_format_layout() {
        // Type=3, Flags=0x8 (hint), Length=0x12345
        let hdr = MessageHeader {
            message_type: 3,
            flags: MessageFlags { hint: true },
            length: 0x1_2345,
        };
        let mut buf = [0u8; 4];
        encode_header(&hdr, &mut buf);
        assert_eq!(buf[0], 3); // type
        assert_eq!(buf[1], 0x81); // flags=0x8 << 4 | length>>16 = 0x80 | 0x01
        assert_eq!(buf[2], 0x23); // length bits 15..8
        assert_eq!(buf[3], 0x45); // length bits 7..0
    }

    #[test]
    fn all_message_types_round_trip() {
        for t in [0u8, 1, 2, 3, 4, 5, 0x70, 0x71, 0x72, 0x73, 0xFF] {
            let hdr = MessageHeader {
                message_type: t,
                flags: MessageFlags::default(),
                length: 100,
            };
            let mut buf = [0u8; 4];
            encode_header(&hdr, &mut buf);
            let decoded = decode_header(&buf).unwrap();
            assert_eq!(decoded.message_type, t);
        }
    }
}
