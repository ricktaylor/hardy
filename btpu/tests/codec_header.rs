//! Message header encode/decode through the public `codec::header` API.

use hardy_btpu::codec::{
    Error,
    header::{
        ContentLength, HEADER_SIZE, MAX_CONTENT_LENGTH, MessageHeader, decode_header, encode_header,
    },
    message::MessageFlags,
};

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
        Err(Error::LengthOverflow {
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
        Err(Error::InsufficientData {
            needed: HEADER_SIZE,
            available: 2,
        })
    );
    assert_eq!(
        decode_header(&[]),
        Err(Error::InsufficientData {
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
    let buf = encode_header(&hdr);
    assert_eq!(buf, [3, 0x81, 0x23, 0x45]);
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
