//! Hint item encode/decode through the public `codec::hint` API.

mod common;

use bytes::{BufMut, Bytes, BytesMut};
use hardy_btpu::codec::{
    Error,
    hint::{
        HINT_HEADER_SIZE, HintItem, HintType, HintValue, Hints, MAX_HINT_VALUE_LEN, decode_hints,
        encode_hints, encoded_hints_len,
    },
};

use self::common::unknown_hint;

fn round_trip(hints: Vec<HintItem>) {
    let mut buf = BytesMut::new();
    encode_hints(&hints, &mut buf);
    let bytes = buf.freeze();
    let (decoded, consumed) = decode_hints(&bytes).unwrap();
    assert_eq!(consumed, bytes.len());
    assert_eq!(decoded, hints);
}

#[test]
fn hint_value_holds_at_most_what_the_length_field_declares() {
    // 256 bytes cannot be declared by the 8-bit length field.
    assert_eq!(
        HintValue::new(Bytes::from(vec![0u8; MAX_HINT_VALUE_LEN + 1])),
        None
    );

    // Exactly 255 bytes is fine and round-trips.
    let value = HintValue::new(Bytes::from(vec![0u8; MAX_HINT_VALUE_LEN])).unwrap();
    round_trip(vec![HintItem::Unknown {
        hint_type: HintType::new(0x2A).unwrap(),
        value,
    }]);
}

#[test]
fn hint_type_holds_at_most_seven_bits() {
    // Types above 0x7F would lose their top bit to the << 1 shift.
    assert_eq!(HintType::new(0x80), None);
    assert_eq!(HintType::new(0x7F), Some(HintType::MAX));

    // Exactly 0x7F is fine and round-trips.
    round_trip(vec![unknown_hint(HintType::MAX.get(), b"x")]);
}

#[test]
fn hint_type_reports_the_wire_type() {
    assert_eq!(
        HintItem::BundleLength(42).hint_type(),
        HintType::BUNDLE_LENGTH
    );
    assert_eq!(
        unknown_hint(0x41, b"x").hint_type(),
        HintType::new(0x41).unwrap()
    );
    // A malformed Bundle Length is carried as unknown under its own type.
    assert_eq!(unknown_hint(0, b"abc").hint_type(), HintType::BUNDLE_LENGTH);
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
        encode_hints(&[HintItem::BundleLength(len)], &mut buf);
        let mut expected = vec![HintType::BUNDLE_LENGTH.get() << 1, width];
        expected.extend_from_slice(&len.to_be_bytes()[8 - usize::from(width)..]);
        assert_eq!(buf[..], expected[..], "Bundle Length {len:#x}");
    }
}

#[test]
fn round_trip_chained_hints() {
    let hints = vec![HintItem::BundleLength(42), unknown_hint(5, b"\x01\x02\x03")];
    let mut buf = BytesMut::new();
    encode_hints(&hints, &mut buf);

    // H is set on every item but the last.  BundleLength(42) takes a
    // one-byte value.
    assert_eq!(buf[0] & 1, 1);
    assert_eq!(buf[HINT_HEADER_SIZE + 1] & 1, 0);

    round_trip(hints);
}

#[test]
fn encoded_len_matches_actual() {
    let hints = vec![HintItem::BundleLength(2000), unknown_hint(10, b"test")];
    let expected = encoded_hints_len(&hints);
    let mut buf = BytesMut::new();
    encode_hints(&hints, &mut buf);
    assert_eq!(buf.len(), expected);
}

#[test]
fn malformed_bundle_length_size_is_carried_as_unknown() {
    // Section 9.1 requires a value length of 1, 2, 4, or 8.  A 3-byte
    // value is a sender fault, but the item is fully framed, so it is
    // carried opaquely rather than failing the message.
    let bytes = Bytes::from_static(&[
        0, // type=0 (Bundle Length), H=0
        3, // length=3 (invalid)
        0x01, 0x02, 0x03,
    ]);
    let (decoded, consumed) = decode_hints(&bytes).unwrap();
    assert_eq!(consumed, bytes.len());
    assert_eq!(
        decoded,
        vec![unknown_hint(HintType::BUNDLE_LENGTH.get(), &[1, 2, 3])]
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
    buf.put_slice(&[(HintType::BUNDLE_LENGTH.get() << 1) | 1, 1, 42]);
    buf.put_slice(&[5 << 1, 1, b'b']);
    let bytes = buf.freeze();
    let (decoded, consumed) = decode_hints(&bytes).unwrap();
    assert_eq!(consumed, bytes.len());
    assert_eq!(
        decoded,
        vec![unknown_hint(5, b"b"), HintItem::BundleLength(42),]
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
    assert_eq!(decoded, vec![unknown_hint(7, &[0xEE])]);
}

#[test]
fn unknown_hint_preserved() {
    round_trip(vec![unknown_hint(0x7F, b"\xDE\xAD")]);
}

#[test]
fn hints_keep_one_item_per_type_latest_wins_in_type_order() {
    let hints = Hints::from(vec![
        unknown_hint(0x41, b"old"),
        unknown_hint(0x07, b"x"),
        HintItem::BundleLength(10),
        unknown_hint(0x41, b"new"),
    ]);
    assert_eq!(hints.len(), 3);
    assert_eq!(
        hints.iter().collect::<Vec<_>>(),
        vec![
            HintItem::BundleLength(10),
            unknown_hint(0x07, b"x"),
            unknown_hint(0x41, b"new"),
        ]
    );
    assert_eq!(hints.clone().into_vec(), hints.iter().collect::<Vec<_>>());
    assert_eq!(hints.into_iter().count(), 3);
}

#[test]
fn hints_compare_by_items_not_insertion_order() {
    let a = Hints::from(vec![unknown_hint(0x07, b"x"), unknown_hint(0x41, b"y")]);
    let b = Hints::from(vec![unknown_hint(0x41, b"y"), unknown_hint(0x07, b"x")]);
    assert_eq!(a, b);
}

#[test]
fn malformed_bundle_length_and_bundle_length_replace_each_other() {
    // A type-0 item of a length Section 9.1 does not allow is carried
    // opaquely, and it replaces a well-formed Bundle Length...
    let hints = Hints::from(vec![HintItem::BundleLength(10), unknown_hint(0, b"abc")]);
    assert_eq!(hints.bundle_length(), None);
    assert_eq!(
        hints.get(HintType::BUNDLE_LENGTH),
        Some(unknown_hint(0, b"abc"))
    );
    assert_eq!(hints.len(), 1);

    // ...as a well-formed one replaces it.
    let hints = Hints::from(vec![unknown_hint(0, b"abc"), HintItem::BundleLength(10)]);
    assert_eq!(hints.bundle_length(), Some(10));
    assert_eq!(
        hints.get(HintType::BUNDLE_LENGTH),
        Some(HintItem::BundleLength(10))
    );
    assert_eq!(hints.len(), 1);
}

#[test]
fn hints_get_and_remove_by_type() {
    let mut hints = Hints::from(vec![HintItem::BundleLength(10), unknown_hint(0x41, b"c")]);
    let correlator = HintType::new(0x41).unwrap();
    let absent = HintType::new(0x42).unwrap();
    assert_eq!(hints.get(correlator), Some(unknown_hint(0x41, b"c")));
    assert_eq!(hints.get(absent), None);

    assert_eq!(hints.remove(absent), None);
    assert_eq!(
        hints.remove(HintType::BUNDLE_LENGTH),
        Some(HintItem::BundleLength(10))
    );
    assert_eq!(hints.remove(correlator), Some(unknown_hint(0x41, b"c")));
    assert!(hints.is_empty());
    assert_eq!(hints, Hints::new());
}

#[test]
fn hints_encoded_len_matches_the_encoder() {
    for hints in [
        Hints::new(),
        Hints::from(vec![HintItem::BundleLength(0x1_0000)]),
        Hints::from(vec![
            HintItem::BundleLength(u64::MAX),
            unknown_hint(0x41, b"corr"),
            unknown_hint(0x7F, b""),
        ]),
    ] {
        let mut buf = BytesMut::new();
        encode_hints(&hints.clone().into_vec(), &mut buf);
        assert_eq!(hints.encoded_len(), buf.len());
    }
}
