//! Hint items and hint sets through the public `codec::hint` API.

mod common;

use bytes::Bytes;
use hardy_btpu::codec::hint::{HintItem, HintType, HintValue, Hints, MAX_HINT_VALUE_LEN};

use self::common::unknown_hint;

#[test]
fn hint_value_holds_at_most_what_the_length_field_declares() {
    // 256 bytes cannot be declared by the 8-bit length field.
    assert_eq!(
        HintValue::new(Bytes::from(vec![0u8; MAX_HINT_VALUE_LEN + 1])),
        None
    );

    // Exactly 255 bytes is fine.
    let value = HintValue::new(Bytes::from(vec![0u8; MAX_HINT_VALUE_LEN])).unwrap();
    assert_eq!(value.as_bytes().len(), MAX_HINT_VALUE_LEN);
}

#[test]
fn hint_type_holds_at_most_seven_bits() {
    // Types above 0x7F would lose their top bit to the << 1 shift.
    assert_eq!(HintType::new(0x80), None);
    assert_eq!(HintType::new(0x7F), Some(HintType::MAX));
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
fn hints_from_a_vec_agrees_with_inserting_one_by_one() {
    let items = vec![
        unknown_hint(0x41, b"old"),
        HintItem::BundleLength(10),
        unknown_hint(0x07, b"x"),
        unknown_hint(0, b"abc"),
        unknown_hint(0x41, b"new"),
        unknown_hint(0x07, b"y"),
    ];
    let mut inserted = Hints::new();
    for item in items.clone() {
        inserted.insert(item);
    }
    let mut extended = Hints::new();
    extended.extend(items.clone());
    assert_eq!(Hints::from(items), inserted);
    assert_eq!(extended, inserted);
    assert_eq!(
        inserted.into_vec(),
        vec![
            unknown_hint(0, b"abc"),
            unknown_hint(0x07, b"y"),
            unknown_hint(0x41, b"new"),
        ]
    );
}

#[test]
fn hints_contain_an_item_only_with_its_value() {
    let hints = Hints::from(vec![HintItem::BundleLength(10), unknown_hint(0x41, b"c")]);
    assert!(hints.contains(&HintItem::BundleLength(10)));
    assert!(!hints.contains(&HintItem::BundleLength(11)));
    assert!(hints.contains(&unknown_hint(0x41, b"c")));
    assert!(!hints.contains(&unknown_hint(0x41, b"d")));
    assert!(!hints.contains(&unknown_hint(0x42, b"c")));
    // A malformed type-0 item is not the well-formed Bundle Length.
    assert!(!hints.contains(&unknown_hint(0, b"\x0a")));
}
