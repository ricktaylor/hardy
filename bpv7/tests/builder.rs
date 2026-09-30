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
