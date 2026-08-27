//! Hint item encode/decode through the public `codec::hint` API.

use bytes::{Bytes, BytesMut};
use hardy_btpu::codec::{
    Error,
    hint::{
        BUNDLE_LENGTH, HINT_HEADER_SIZE, HintItem, MAX_HINT_TYPE, MAX_HINT_VALUE_LEN, decode_hints,
        encode_hints, encoded_hints_len,
    },
};

#[test]
fn oversized_hint_value_rejected() {
    // 256-byte value cannot be represented by the 8-bit length field;
    // it must error rather than truncate into a corrupt chain.
    let hints = vec![HintItem::Unknown {
        hint_type: 0x2A,
        value: Bytes::from(vec![0u8; MAX_HINT_VALUE_LEN + 1]),
    }];
    let mut buf = BytesMut::new();
    let err = encode_hints(&hints, &mut buf).unwrap_err();
    assert!(matches!(err, Error::HintValueOverflow { length: 256, .. }));
    assert!(buf.is_empty());

    // Exactly 255 bytes is fine and round-trips.
    let hints = vec![HintItem::Unknown {
        hint_type: 0x2A,
        value: Bytes::from(vec![0u8; MAX_HINT_VALUE_LEN]),
    }];
    let mut buf = BytesMut::new();
    encode_hints(&hints, &mut buf).unwrap();
    let bytes = buf.freeze();
    let (decoded, _) = decode_hints(&bytes, &bytes).unwrap();
    assert_eq!(decoded, hints);
}

#[test]
fn oversized_hint_type_rejected() {
    // Types above 0x7F would lose their top bit to the << 1 shift.
    let hints = vec![HintItem::Unknown {
        hint_type: 0x80,
        value: Bytes::from_static(b"x"),
    }];
    let mut buf = BytesMut::new();
    let err = encode_hints(&hints, &mut buf).unwrap_err();
    assert!(matches!(err, Error::InvalidHintType(0x80)));
    assert!(buf.is_empty());

    // Exactly 0x7F is fine and round-trips.
    let hints = vec![HintItem::Unknown {
        hint_type: MAX_HINT_TYPE,
        value: Bytes::from_static(b"x"),
    }];
    let mut buf = BytesMut::new();
    encode_hints(&hints, &mut buf).unwrap();
    let bytes = buf.freeze();
    let (decoded, _) = decode_hints(&bytes, &bytes).unwrap();
    assert_eq!(decoded, hints);
}

#[test]
fn round_trip_bundle_length_1byte() {
    let hints = vec![HintItem::BundleLength(200)];
    let mut buf = BytesMut::new();
    encode_hints(&hints, &mut buf).unwrap();
    let bytes = buf.freeze();
    let (decoded, consumed) = decode_hints(&bytes, &bytes).unwrap();
    assert_eq!(consumed, bytes.len());
    assert_eq!(decoded, hints);
}

#[test]
fn round_trip_bundle_length_2byte() {
    let hints = vec![HintItem::BundleLength(2000)];
    let mut buf = BytesMut::new();
    encode_hints(&hints, &mut buf).unwrap();
    let bytes = buf.freeze();
    let (decoded, consumed) = decode_hints(&bytes, &bytes).unwrap();
    assert_eq!(consumed, bytes.len());
    assert_eq!(decoded, hints);
}

#[test]
fn round_trip_bundle_length_4byte() {
    let hints = vec![HintItem::BundleLength(100_000)];
    let mut buf = BytesMut::new();
    encode_hints(&hints, &mut buf).unwrap();
    let bytes = buf.freeze();
    let (decoded, consumed) = decode_hints(&bytes, &bytes).unwrap();
    assert_eq!(consumed, bytes.len());
    assert_eq!(decoded, hints);
}

#[test]
fn round_trip_bundle_length_8byte() {
    let hints = vec![HintItem::BundleLength(u64::MAX)];
    let mut buf = BytesMut::new();
    encode_hints(&hints, &mut buf).unwrap();
    let bytes = buf.freeze();
    let (decoded, consumed) = decode_hints(&bytes, &bytes).unwrap();
    assert_eq!(consumed, bytes.len());
    assert_eq!(decoded, hints);
}

#[test]
fn round_trip_chained_hints() {
    let hints = vec![
        HintItem::BundleLength(42),
        HintItem::Unknown {
            hint_type: 5,
            value: Bytes::from_static(b"\x01\x02\x03"),
        },
    ];
    let mut buf = BytesMut::new();
    encode_hints(&hints, &mut buf).unwrap();

    // First hint should have H=1 (more follow)
    assert_eq!(buf[0] & 1, 1);
    // Second hint should have H=0 (last)
    let first_total = HINT_HEADER_SIZE + 1; // BundleLength(42) = 1 byte value
    assert_eq!(buf[first_total] & 1, 0);

    let bytes = buf.freeze();
    let (decoded, consumed) = decode_hints(&bytes, &bytes).unwrap();
    assert_eq!(consumed, bytes.len());
    assert_eq!(decoded, hints);
}

#[test]
fn encoded_len_matches_actual() {
    let hints = vec![
        HintItem::BundleLength(2000),
        HintItem::Unknown {
            hint_type: 10,
            value: Bytes::from_static(b"test"),
        },
    ];
    let expected = encoded_hints_len(&hints);
    let mut buf = BytesMut::new();
    encode_hints(&hints, &mut buf).unwrap();
    assert_eq!(buf.len(), expected);
}

#[test]
fn invalid_bundle_length_size() {
    // Manually construct a hint with an invalid 3-byte bundle length
    let data = [
        (BUNDLE_LENGTH << 1), // type=0, H=0
        3,                    // length=3 (invalid)
        0x01,
        0x02,
        0x03, // value
    ];
    let bytes = Bytes::copy_from_slice(&data);
    let result = decode_hints(&bytes, &bytes);
    assert!(result.is_err());
}

#[test]
fn unknown_hint_preserved() {
    let hints = vec![HintItem::Unknown {
        hint_type: 0x7F,
        value: Bytes::from_static(b"\xDE\xAD"),
    }];
    let mut buf = BytesMut::new();
    encode_hints(&hints, &mut buf).unwrap();
    let bytes = buf.freeze();
    let (decoded, _) = decode_hints(&bytes, &bytes).unwrap();
    assert_eq!(decoded, hints);
}
