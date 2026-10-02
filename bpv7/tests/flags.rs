//! Integration tests for the RFC 9171 codes bpv7 names — `block::Flags`
//! and `bundle::Flags` against the bit assignments of §4.2.4 and §4.2.3,
//! and `block::Type` against §4.2.1's type codes — and for their equality,
//! hashing, and serde, which all go by the encoding. The BPSec scope flags'
//! walk lives with their codec in `tests/rfc9173.rs`.

use core::cmp::Ordering;
use std::hash::{DefaultHasher, Hash, Hasher};

use hardy_bpv7::{block, bundle};

fn hash_of<T: Hash>(value: &T) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

// The block processing control flags bpv7 names are exactly RFC 9171
// §4.2.4's, each decoding to the field the RFC names it: every other bit
// round-trips as unrecognised, and a named bit carried in `unrecognised`
// encodes as its bit and canonicalizes to its field.
#[test]
fn block_flags_name_exactly_the_rfc_bits() {
    let named = [
        (
            0,
            block::Flags {
                must_replicate: true,
                ..Default::default()
            },
        ),
        (
            1,
            block::Flags {
                report_on_failure: true,
                ..Default::default()
            },
        ),
        (
            2,
            block::Flags {
                delete_bundle_on_failure: true,
                ..Default::default()
            },
        ),
        (
            4,
            block::Flags {
                delete_block_on_failure: true,
                ..Default::default()
            },
        ),
    ];
    for bit in 0..64 {
        let value = 1u64 << bit;
        let decoded = block::Flags::from(value);
        assert!(
            decoded.is_canonical(),
            "bit {bit}: a decoded value is canonical"
        );
        let named_flags = named
            .iter()
            .find(|(named_bit, _)| *named_bit == bit)
            .map(|(_, flags)| flags.clone());
        let expected = named_flags.clone().unwrap_or(block::Flags {
            unrecognised: value,
            ..Default::default()
        });
        // Both canonical, so equal encodings mean equal fields.
        assert_eq!(
            decoded, expected,
            "bit {bit}: decodes to the field RFC 9171 names, or as unrecognised"
        );
        assert_eq!(u64::from(&decoded), value, "bit {bit} round-trips");
        let alias = block::Flags {
            unrecognised: value,
            ..Default::default()
        };
        assert_eq!(
            u64::from(&alias),
            value,
            "bit {bit}: an alias encodes as its bit"
        );
        assert_eq!(
            alias.is_canonical(),
            named_flags.is_none(),
            "bit {bit}: an alias of a named bit is not canonical"
        );
        let folded = alias.clone().canonicalize();
        assert!(folded.is_canonical(), "bit {bit}: the alias folds");
        assert_eq!(
            folded, decoded,
            "bit {bit}: the alias folds to the decoded value"
        );
        assert_eq!(alias, decoded, "bit {bit}: an alias equals what it encodes");
        assert_eq!(
            hash_of(&alias),
            hash_of(&decoded),
            "bit {bit}: and hashes alike"
        );
    }
}

// The bundle processing control flags bpv7 names are exactly RFC 9171
// §4.2.3's, with the same field, round-trip and alias rules.
#[test]
fn bundle_flags_name_exactly_the_rfc_bits() {
    let named = [
        (
            0,
            bundle::Flags {
                is_fragment: true,
                ..Default::default()
            },
        ),
        (
            1,
            bundle::Flags {
                is_admin_record: true,
                ..Default::default()
            },
        ),
        (
            2,
            bundle::Flags {
                do_not_fragment: true,
                ..Default::default()
            },
        ),
        (
            5,
            bundle::Flags {
                app_ack_requested: true,
                ..Default::default()
            },
        ),
        (
            6,
            bundle::Flags {
                report_status_time: true,
                ..Default::default()
            },
        ),
        (
            14,
            bundle::Flags {
                receipt_report_requested: true,
                ..Default::default()
            },
        ),
        (
            16,
            bundle::Flags {
                forward_report_requested: true,
                ..Default::default()
            },
        ),
        (
            17,
            bundle::Flags {
                delivery_report_requested: true,
                ..Default::default()
            },
        ),
        (
            18,
            bundle::Flags {
                delete_report_requested: true,
                ..Default::default()
            },
        ),
    ];
    for bit in 0..64 {
        let value = 1u64 << bit;
        let decoded = bundle::Flags::from(value);
        assert!(
            decoded.is_canonical(),
            "bit {bit}: a decoded value is canonical"
        );
        let named_flags = named
            .iter()
            .find(|(named_bit, _)| *named_bit == bit)
            .map(|(_, flags)| flags.clone());
        let expected = named_flags.clone().unwrap_or(bundle::Flags {
            unrecognised: value,
            ..Default::default()
        });
        // Both canonical, so equal encodings mean equal fields.
        assert_eq!(
            decoded, expected,
            "bit {bit}: decodes to the field RFC 9171 names, or as unrecognised"
        );
        assert_eq!(u64::from(&decoded), value, "bit {bit} round-trips");
        let alias = bundle::Flags {
            unrecognised: value,
            ..Default::default()
        };
        assert_eq!(
            u64::from(&alias),
            value,
            "bit {bit}: an alias encodes as its bit"
        );
        assert_eq!(
            alias.is_canonical(),
            named_flags.is_none(),
            "bit {bit}: an alias of a named bit is not canonical"
        );
        let folded = alias.clone().canonicalize();
        assert!(folded.is_canonical(), "bit {bit}: the alias folds");
        assert_eq!(
            folded, decoded,
            "bit {bit}: the alias folds to the decoded value"
        );
        assert_eq!(alias, decoded, "bit {bit}: an alias equals what it encodes");
        assert_eq!(
            hash_of(&alias),
            hash_of(&decoded),
            "bit {bit}: and hashes alike"
        );
    }
}

// Block types compare, order, and hash by their RFC 9171 §4.2.1 code: an
// `Unrecognised` alias of a known code equals the variant it encodes as,
// and the order is the code order.
#[test]
fn block_types_compare_by_code() {
    for (alias, named) in [
        (block::Type::Unrecognised(1), block::Type::Payload),
        (block::Type::Unrecognised(10), block::Type::HopCount),
        (block::Type::Unrecognised(11), block::Type::BlockIntegrity),
    ] {
        assert_eq!(alias, named);
        assert_eq!(alias.cmp(&named), Ordering::Equal);
        assert_eq!(hash_of(&alias), hash_of(&named));
        assert!(!alias.is_canonical());
        assert!(named.is_canonical());
    }
    assert!(block::Type::Unrecognised(2) < block::Type::PreviousNode);
    assert!(block::Type::HopCount < block::Type::BlockIntegrity);
    assert!(block::Type::BlockSecurity < block::Type::Unrecognised(13));
}

// Serde canonicalizes in both directions: an alias in a stored record loads
// as the flag it encodes, and a non-canonical value stores canonically, in
// the plain field-list shape.
#[test]
fn flags_serde_canonicalizes_both_ways() {
    let loaded: block::Flags = serde_json::from_str(r#"{"unrecognised":2}"#).unwrap();
    assert!(loaded.is_canonical());
    assert!(loaded.report_on_failure);
    let stored = serde_json::to_string(&block::Flags {
        unrecognised: (1 << 1) | (1 << 8),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(stored, r#"{"report_on_failure":true,"unrecognised":256}"#);

    let loaded: bundle::Flags = serde_json::from_str(r#"{"unrecognised":2}"#).unwrap();
    assert!(loaded.is_canonical());
    assert!(loaded.is_admin_record);
    let stored = serde_json::to_string(&bundle::Flags {
        unrecognised: (1 << 1) | (1 << 24),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        stored,
        r#"{"is_admin_record":true,"unrecognised":16777216}"#
    );

    // `unrecognised` is a plain integer: an explicit null is a data error.
    for null in [
        serde_json::from_str::<block::Flags>(r#"{"unrecognised":null}"#).map(drop),
        serde_json::from_str::<bundle::Flags>(r#"{"unrecognised":null}"#).map(drop),
    ] {
        let Err(e) = null else {
            panic!("an explicit null must not deserialize");
        };
        assert!(matches!(e.classify(), serde_json::error::Category::Data));
    }
}

#[test]
fn block_type_serde_canonicalizes_both_ways() {
    let loaded: block::Type = serde_json::from_str(r#"{"Unrecognised":11}"#).unwrap();
    assert!(matches!(loaded, block::Type::BlockIntegrity));
    assert_eq!(
        serde_json::to_string(&block::Type::Unrecognised(10)).unwrap(),
        r#""HopCount""#
    );
    assert_eq!(
        serde_json::to_string(&block::Type::Unrecognised(200)).unwrap(),
        r#"{"Unrecognised":200}"#
    );
}
