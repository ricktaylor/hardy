//! Message header encode/decode through the public `codec::header` API.

use hardy_btpu::codec::{
    header::{MAX_CONTENT_LENGTH, MessageHeader, decode_header, encode_header},
    message::MessageFlags,
};

#[test]
fn round_trip_basic() {
    let hdr = MessageHeader {
        message_type: 3,
        flags: MessageFlags::default(),
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
        flags: MessageFlags { hint: true, rfu: 0 },
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
        flags: MessageFlags::default(),
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
        flags: MessageFlags::default(),
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
        flags: MessageFlags { hint: true, rfu: 0 },
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
