//! Integration tests for `hardy_bpv7::builder` — the flags a built bundle
//! carries on the wire, read back through the parser.

use core::num::NonZeroU8;

use bytes::Bytes;
use hardy_bpv7::{block, builder, bundle, creation_timestamp, hop_info, parse};

// A bundle that forbids `report_on_failure` (RFC 9171 §4.2.3-4/-5) is built
// with the flag clear on every block — the Hop Count block's default and
// caller-set flags on an extension block and on the payload alike — so it
// parses; an ordinary bundle keeps all three.
#[test]
fn builder_clears_report_on_failure_on_a_forbidding_bundle() {
    let hop_info = hop_info::HopInfo {
        limit: NonZeroU8::new(30).unwrap(),
        count: 0,
    };
    let reporting = || block::Flags {
        report_on_failure: true,
        ..Default::default()
    };
    for (label, source, flags, reports) in [
        (
            "null source",
            "dtn:none",
            bundle::Flags {
                do_not_fragment: true,
                ..Default::default()
            },
            false,
        ),
        (
            "admin record",
            "ipn:1.0",
            bundle::Flags {
                is_admin_record: true,
                ..Default::default()
            },
            false,
        ),
        ("ordinary", "ipn:1.0", bundle::Flags::default(), true),
    ] {
        let (_, data) = builder::Builder::new(source.parse().unwrap(), "ipn:2.0".parse().unwrap())
            .with_flags(flags)
            .with_hop_count(&hop_info)
            .add_extension_block(block::Type::Unrecognised(200))
            .unwrap()
            .with_flags(reporting())
            .build(b"ext-data".as_slice().into())
            .add_extension_block(block::Type::Payload)
            .unwrap()
            .with_flags(reporting())
            .build(b"Hello".as_slice().into())
            .build(creation_timestamp::CreationTimestamp::now())
            .unwrap();
        let parsed = parse::parse(Bytes::from(data))
            .unwrap_or_else(|e| panic!("{label} bundle must parse: {e:?}"))
            .bundle;
        for block_type in [
            block::Type::HopCount,
            block::Type::Unrecognised(200),
            block::Type::Payload,
        ] {
            let block = parsed
                .blocks
                .values()
                .find(|b| b.block_type == block_type)
                .expect("the block is present");
            assert_eq!(
                block.flags.report_on_failure, reports,
                "{label}: {block_type:?} reports on failure exactly when the bundle allows it"
            );
        }
    }
}

// The builder canonicalizes the flags it is handed, at both flag levels: a
// named bit carried in `unrecognised` is the flag it encodes — the
// admin-record alias makes an administrative record, whose normalisation
// then clears the Hop Count's `report_on_failure`, and `report_on_failure`'s
// alias reports on an ordinary bundle's block — while genuinely unrecognised
// bits pass through. The returned `Bundle` holds what the bytes hold.
#[test]
fn builder_canonicalizes_unrecognised_aliases_of_named_flags() {
    let (built, data) =
        builder::Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
            .with_flags(bundle::Flags {
                unrecognised: (1 << 1) | (1 << 24),
                ..Default::default()
            })
            .with_hop_count(&hop_info::HopInfo {
                limit: NonZeroU8::new(30).unwrap(),
                count: 0,
            })
            .with_payload(b"Hello".as_slice().into())
            .build(creation_timestamp::CreationTimestamp::now())
            .unwrap();
    let parsed = parse::parse(Bytes::from(data))
        .expect("the administrative record's blocks request no report")
        .bundle;
    assert!(parsed.primary.flags.is_admin_record);
    assert_eq!(parsed.primary.flags.unrecognised, 1 << 24);
    assert!(built.primary.flags.is_canonical());
    assert_eq!(built.primary.flags, parsed.primary.flags);
    let hop_count = parsed
        .blocks
        .values()
        .find(|b| b.block_type == block::Type::HopCount)
        .expect("the Hop Count block is present");
    assert!(
        !hop_count.flags.report_on_failure,
        "an administrative record clears the Hop Count default"
    );

    for (label, bundle_flags, reports) in [
        ("ordinary", bundle::Flags::default(), true),
        (
            "admin record",
            bundle::Flags {
                is_admin_record: true,
                ..Default::default()
            },
            false,
        ),
    ] {
        let (built, data) =
            builder::Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
                .with_flags(bundle_flags)
                .add_extension_block(block::Type::Unrecognised(200))
                .unwrap()
                .with_flags(block::Flags {
                    unrecognised: (1 << 1) | (1 << 8),
                    ..Default::default()
                })
                .build(b"ext-data".as_slice().into())
                .with_payload(b"Hello".as_slice().into())
                .build(creation_timestamp::CreationTimestamp::now())
                .unwrap();
        let parsed = parse::parse(Bytes::from(data))
            .unwrap_or_else(|e| panic!("{label}: the bundle must parse: {e:?}"))
            .bundle;
        let block = parsed
            .blocks
            .values()
            .find(|b| b.block_type == block::Type::Unrecognised(200))
            .unwrap_or_else(|| panic!("{label}: the extension block is present"));
        assert_eq!(
            block.flags.report_on_failure, reports,
            "{label}: the alias reports exactly when the bundle allows it"
        );
        assert_eq!(block.flags.unrecognised, 1 << 8, "{label}");
        let built_block = built
            .blocks
            .values()
            .find(|b| b.block_type == block::Type::Unrecognised(200))
            .unwrap_or_else(|| panic!("{label}: the built view has the block"));
        assert!(built_block.flags.is_canonical(), "{label}");
        assert_eq!(built_block.flags, block.flags, "{label}");
    }
}
