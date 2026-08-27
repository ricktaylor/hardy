//! Message header encode/decode through the public `codec::header` API.

use hardy_btpu::codec::{
    Error,
    header::{HEADER_SIZE, MAX_CONTENT_LENGTH, MessageHeader, decode_header, encode_header},
    message::MessageFlags,
};

#[test]
fn round_trip_basic() {
    let hdr = MessageHeader {
        message_type: 3,
        flags: MessageFlags::default(),
        length: 256,
    };
    let buf = encode_header(&hdr).unwrap();
    assert_eq!(decode_header(&buf), Ok(hdr));
}

#[test]
fn round_trip_with_hint_flag() {
    let hdr = MessageHeader {
        message_type: 2,
        flags: MessageFlags { hint: true, rfu: 0 },
        length: 42,
    };
    let buf = encode_header(&hdr).unwrap();
    assert_eq!(decode_header(&buf), Ok(hdr));
}

#[test]
fn round_trip_max_length() {
    let hdr = MessageHeader {
        message_type: 1,
        flags: MessageFlags::default(),
        length: MAX_CONTENT_LENGTH as u32,
    };
    let buf = encode_header(&hdr).unwrap();
    assert_eq!(decode_header(&buf), Ok(hdr));
}

#[test]
fn length_above_20_bits_is_refused() {
    // The field cannot hold it; truncating would emit a header claiming
    // a different length.
    let hdr = MessageHeader {
        message_type: 2,
        flags: MessageFlags::default(),
        length: MAX_CONTENT_LENGTH as u32 + 1,
    };
    assert_eq!(
        encode_header(&hdr),
        Err(Error::LengthOverflow {
            length: MAX_CONTENT_LENGTH + 1,
            max: MAX_CONTENT_LENGTH,
        })
    );
}

#[test]
fn round_trip_zero_length() {
    let hdr = MessageHeader {
        message_type: 5,
        flags: MessageFlags::default(),
        length: 0,
    };
    let buf = encode_header(&hdr).unwrap();
    assert_eq!(decode_header(&buf), Ok(hdr));
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
        length: 0x1_2345,
    };
    let buf = encode_header(&hdr).unwrap();
    assert_eq!(buf, [3, 0x81, 0x23, 0x45]);
}

#[test]
fn all_message_types_round_trip() {
    for t in [0u8, 1, 2, 3, 4, 5, 0x70, 0x71, 0x72, 0x73, 0xFF] {
        let hdr = MessageHeader {
            message_type: t,
            flags: MessageFlags::default(),
            length: 100,
        };
        let buf = encode_header(&hdr).unwrap();
        assert_eq!(decode_header(&buf).unwrap().message_type, t);
    }
}
