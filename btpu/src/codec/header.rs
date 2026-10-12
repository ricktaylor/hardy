//! The four-byte message header (Section 7).

use crate::codec::{DecodeError, EncodeError, message::MessageFlags};

/// Size of the standard message header in bytes.
pub const HEADER_SIZE: usize = 4;

/// Maximum value of the 20-bit content-length field.
pub const MAX_CONTENT_LENGTH: usize = 0xF_FFFF; // 1,048,575

/// A message content length: a value of the header's 20-bit Length field
/// (Section 7), so at most [`MAX_CONTENT_LENGTH`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ContentLength(usize);

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
    type Error = EncodeError;

    /// Errors with [`EncodeError::LengthOverflow`] if `len` exceeds
    /// [`MAX_CONTENT_LENGTH`].
    fn try_from(len: usize) -> Result<Self, EncodeError> {
        Self::new(len).ok_or(EncodeError::LengthOverflow {
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
pub(crate) struct MessageHeader {
    /// The message type value (Section 12.1 registry).
    pub message_type: u8,
    /// The four flag bits (Section 7.1).
    pub flags: MessageFlags,
    /// The content length in bytes.
    pub length: ContentLength,
}

/// Encode a message header into its 4-byte wire form.
pub(crate) fn encode_header(header: &MessageHeader) -> [u8; HEADER_SIZE] {
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
/// Errors with [`DecodeError::InsufficientData`] if `src` is shorter than
/// [`HEADER_SIZE`].
pub(crate) fn decode_header(src: &[u8]) -> Result<MessageHeader, DecodeError> {
    let [message_type, flags_high, mid, low, ..] = *src else {
        return Err(DecodeError::InsufficientData {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn content_length(len: usize) -> ContentLength {
        ContentLength::new(len).unwrap()
    }

    fn assert_round_trips(message_type: u8, flags: MessageFlags, length: usize) {
        let hdr = MessageHeader {
            message_type,
            flags,
            length: content_length(length),
        };
        let buf = encode_header(&hdr);
        assert_eq!(decode_header(&buf), Ok(hdr));
    }

    #[test]
    fn round_trip_basic() {
        assert_round_trips(3, MessageFlags::default(), 256);
    }

    #[test]
    fn round_trip_with_hint_flag() {
        assert_round_trips(2, MessageFlags { hint: true, rfu: 0 }, 42);
    }

    #[test]
    fn round_trip_max_length() {
        assert_round_trips(1, MessageFlags::default(), MAX_CONTENT_LENGTH);
    }

    #[test]
    fn length_above_20_bits_is_refused() {
        // The field cannot hold it; truncating would emit a header claiming
        // a different length.
        assert_eq!(ContentLength::new(MAX_CONTENT_LENGTH + 1), None);
        assert_eq!(
            ContentLength::try_from(MAX_CONTENT_LENGTH + 1),
            Err(EncodeError::LengthOverflow {
                length: MAX_CONTENT_LENGTH + 1,
                max: MAX_CONTENT_LENGTH,
            })
        );
        assert_eq!(ContentLength::MAX.get(), MAX_CONTENT_LENGTH);
    }

    #[test]
    fn round_trip_zero_length() {
        assert_round_trips(5, MessageFlags::default(), 0);
    }

    #[test]
    fn decode_insufficient_data() {
        assert_eq!(
            decode_header(&[0, 0]),
            Err(DecodeError::InsufficientData {
                needed: HEADER_SIZE,
                available: 2,
            })
        );
        assert_eq!(
            decode_header(&[]),
            Err(DecodeError::InsufficientData {
                needed: HEADER_SIZE,
                available: 0,
            })
        );
    }

    #[test]
    fn wire_format_layout() {
        // Type=3, Flags=0x8 (hint), Length=0x12345
        let hdr = MessageHeader {
            message_type: 3,
            flags: MessageFlags { hint: true, rfu: 0 },
            length: content_length(0x1_2345),
        };
        assert_eq!(encode_header(&hdr), [3, 0x81, 0x23, 0x45]);
    }

    #[test]
    fn all_message_types_round_trip() {
        for t in [0u8, 1, 2, 3, 4, 5, 0x70, 0x71, 0x72, 0x73, 0xFF] {
            let hdr = MessageHeader {
                message_type: t,
                flags: MessageFlags::default(),
                length: content_length(100),
            };
            let buf = encode_header(&hdr);
            assert_eq!(decode_header(&buf).unwrap().message_type, t);
        }
    }
}
