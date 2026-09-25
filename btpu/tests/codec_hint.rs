//! Hint item encode/decode through the public `codec::hint` API.

use bytes::{BufMut, Bytes, BytesMut};
use hardy_btpu::codec::{
    Error,
    hint::{
        BUNDLE_LENGTH_HINT, HINT_HEADER_SIZE, HintItem, MAX_HINT_TYPE, MAX_HINT_VALUE_LEN,
        ValidationError, decode_hints, encode_hints, encoded_hints_len, validate_hints,
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
    let expected = ValidationError::ValueOverflow {
        length: MAX_HINT_VALUE_LEN + 1,
        max: MAX_HINT_VALUE_LEN,
    };
    assert_eq!(validate_hints(&hints), Err(expected.clone()));
    let mut buf = BytesMut::new();
    assert_eq!(encode_hints(&hints, &mut buf), Err(Error::Hint(expected)));
    assert!(buf.is_empty());

    // Exactly 255 bytes is fine and round-trips.
    let hints = vec![HintItem::Unknown {
        hint_type: 0x2A,
        value: Bytes::from(vec![0u8; MAX_HINT_VALUE_LEN]),
    }];
    let mut buf = BytesMut::new();
    encode_hints(&hints, &mut buf).unwrap();
    let (decoded, _) = decode_hints(&buf.freeze()).unwrap();
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
    assert_eq!(
        encode_hints(&hints, &mut buf),
        Err(Error::Hint(ValidationError::InvalidHintType(0x80)))
    );
    assert!(buf.is_empty());

    // Exactly 0x7F is fine and round-trips.
    let hints = vec![HintItem::Unknown {
        hint_type: MAX_HINT_TYPE,
        value: Bytes::from_static(b"x"),
    }];
    let mut buf = BytesMut::new();
    encode_hints(&hints, &mut buf).unwrap();
    let (decoded, _) = decode_hints(&buf.freeze()).unwrap();
    assert_eq!(decoded, hints);
}

fn round_trip(hints: Vec<HintItem>) {
    let mut buf = BytesMut::new();
    encode_hints(&hints, &mut buf).unwrap();
    let bytes = buf.freeze();
    let (decoded, consumed) = decode_hints(&bytes).unwrap();
    assert_eq!(consumed, bytes.len());
    assert_eq!(decoded, hints);
}

#[test]
fn round_trip_bundle_length_every_width() {
    round_trip(vec![HintItem::BundleLength(200)]);
    round_trip(vec![HintItem::BundleLength(2000)]);
    round_trip(vec![HintItem::BundleLength(100_000)]);
    round_trip(vec![HintItem::BundleLength(u64::MAX)]);
}

#[test]
fn bundle_length_uses_the_shortest_width_either_side_of_each_boundary() {
    for (len, width) in [
        (0, 1),
        (0xFF, 1),
        (0x100, 2),
        (0xFFFF, 2),
        (0x1_0000, 4),
        (0xFFFF_FFFF, 4),
        (0x1_0000_0000, 8),
        (u64::MAX, 8),
    ] {
        let mut buf = BytesMut::new();
        encode_hints(&[HintItem::BundleLength(len)], &mut buf).unwrap();
        let mut expected = vec![BUNDLE_LENGTH_HINT << 1, width];
        expected.extend_from_slice(&len.to_be_bytes()[8 - usize::from(width)..]);
        assert_eq!(buf[..], expected[..], "Bundle Length {len:#x}");
    }
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
    let (decoded, consumed) = decode_hints(&bytes).unwrap();
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
fn malformed_bundle_length_size_is_carried_as_unknown() {
    // Section 9.1 requires a value length of 1, 2, 4, or 8.  A 3-byte
    // value is a sender fault, but the item is fully framed, so it is
    // carried opaquely rather than failing the message.
    let bytes = Bytes::from_static(&[
        BUNDLE_LENGTH_HINT << 1, // type=0, H=0
        3,                       // length=3 (invalid)
        0x01,
        0x02,
        0x03,
    ]);
    let (decoded, consumed) = decode_hints(&bytes).unwrap();
    assert_eq!(consumed, bytes.len());
    assert_eq!(
        decoded,
        vec![HintItem::Unknown {
            hint_type: BUNDLE_LENGTH_HINT,
            value: Bytes::from_static(&[1, 2, 3]),
        }]
    );
}

#[test]
fn truncated_chain_errors() {
    // Header promising a 5-byte value with 2 bytes behind it.
    let bytes = Bytes::from_static(&[0x0A, 5, 0xAA, 0xBB]);
    assert_eq!(
        decode_hints(&bytes),
        Err(Error::InsufficientData {
            needed: 7,
            available: 4,
        })
    );
    // Header cut short.
    let bytes = Bytes::from_static(&[0x0A]);
    assert_eq!(
        decode_hints(&bytes),
        Err(Error::InsufficientData {
            needed: 2,
            available: 1,
        })
    );
}

#[test]
fn repeated_types_fold_latest_wins_in_first_appearance_order() {
    // Chain: type 5 = "a", type 0 = 42, type 5 = "b".  Type 5 keeps its
    // first position but takes the later value.
    let mut buf = BytesMut::new();
    buf.put_slice(&[(5 << 1) | 1, 1, b'a']);
    buf.put_slice(&[(BUNDLE_LENGTH_HINT << 1) | 1, 1, 42]);
    buf.put_slice(&[5 << 1, 1, b'b']);
    let bytes = buf.freeze();
    let (decoded, consumed) = decode_hints(&bytes).unwrap();
    assert_eq!(consumed, bytes.len());
    assert_eq!(
        decoded,
        vec![
            HintItem::Unknown {
                hint_type: 5,
                value: Bytes::from_static(b"b"),
            },
            HintItem::BundleLength(42),
        ]
    );
}

#[test]
fn long_chain_of_repeats_folds_to_one_item() {
    // Ten thousand repeats of one type decode to a single item: the
    // returned Vec is bounded by the type space, not the chain length.
    let mut buf = BytesMut::new();
    for _ in 0..9_999 {
        buf.put_slice(&[(7 << 1) | 1, 0]);
    }
    buf.put_slice(&[7 << 1, 1, 0xEE]);
    let bytes = buf.freeze();
    let (decoded, consumed) = decode_hints(&bytes).unwrap();
    assert_eq!(consumed, bytes.len());
    assert_eq!(
        decoded,
        vec![HintItem::Unknown {
            hint_type: 7,
            value: Bytes::from_static(&[0xEE]),
        }]
    );
}

#[test]
fn unknown_hint_preserved() {
    round_trip(vec![HintItem::Unknown {
        hint_type: 0x7F,
        value: Bytes::from_static(b"\xDE\xAD"),
    }]);
}
