//! Integration tests for `Builder` via the public `hardy_bpv7` API.

use hardy_bpv7::{
    CreationTimestamp,
    builder::{Builder, BundleTemplate},
    bundle::BlockType,
    eid::Eid,
};
use hardy_cbor::encode::emit;

// Requirement: LLR 1.1.25
#[test]
fn builder() {
    Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
        .with_report_to("ipn:3.0".parse().unwrap())
        .with_payload("Hello".as_bytes().into())
        .build(CreationTimestamp::now())
        .unwrap();
}

// Requirement: LLR 1.1.25 — the returned `blocks` map is keyed by wire block
// number (primary 0, payload 1, extensions 2..), matching a parsed bundle.
#[test]
fn builder_block_map_keys() {
    let prev_node: Eid = "ipn:3.0".parse().unwrap();
    let (bundle, _data) = Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
        .add_extension_block(BlockType::PreviousNode)
        .unwrap()
        .build(emit(&prev_node).0.into())
        .add_extension_block(BlockType::BundleAge)
        .unwrap()
        .build(emit(&0u64).0.into())
        .with_payload("Hello".as_bytes().into())
        .build(CreationTimestamp::now())
        .unwrap();

    assert_eq!(bundle.blocks.len(), 4);
    assert_eq!(bundle.blocks[&0].block_type, BlockType::Primary);
    assert_eq!(bundle.blocks[&1].block_type, BlockType::Payload);
    assert_eq!(bundle.blocks[&2].block_type, BlockType::PreviousNode);
    assert_eq!(bundle.blocks[&3].block_type, BlockType::BundleAge);
}

// Requirement: LLR 1.1.25
#[test]
fn template() {
    let b: Builder = serde_json::from_value::<BundleTemplate>(serde_json::json!({
        "source": "ipn:1.0",
        "destination": "ipn:2.0",
        "report_to": "ipn:3.0"
    }))
    .unwrap()
    .into();

    b.with_payload("Hello".as_bytes().into())
        .build(CreationTimestamp::now())
        .unwrap();
}
