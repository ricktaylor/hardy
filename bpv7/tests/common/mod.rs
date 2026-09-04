//! Shared byte-splicing fixtures for the bpv7 integration tests: helpers
//! for hand-crafting malformed wire input. Lives under `tests/common/` so
//! cargo treats it as a module, not its own test binary.
//!
//! Different test binaries use different subsets, so unused helpers per
//! binary are expected.
#![allow(dead_code)]

use hardy_bpv7::eid::Eid;
use hardy_cbor::{
    decode::skip_value,
    encode::{Bytes, Encoder, emit_array},
};

// A generous CBOR nesting bound for skipping a primary block: a flat
// array whose deepest members are EID and timestamp arrays.
const PRIMARY_BLOCK_SKIP_DEPTH: usize = 16;

/// A canonical block `[type, number, flags, crc_type=none, data]`.
pub fn make_block(block_type: u64, block_number: u64, flags: u64, payload: &[u8]) -> Vec<u8> {
    emit_array(Some(5), |a| {
        a.emit(&block_type);
        a.emit(&block_number);
        a.emit(&flags);
        a.emit(&0u64); // CRC type: none
        a.emit(&Bytes(payload));
    })
}

/// Splice raw block bytes in between the primary block and the payload block.
pub fn insert_after_primary(data: &[u8], blocks: &[&[u8]]) -> Vec<u8> {
    assert_eq!(
        data[0], 0x9F,
        "bundle should start with an indefinite array"
    );
    let (_, primary_len) =
        skip_value(&data[1..], PRIMARY_BLOCK_SKIP_DEPTH).expect("should skip the primary block");
    let insert_pos = 1 + primary_len;

    let mut modified = data[..insert_pos].to_vec();
    for block in blocks {
        modified.extend_from_slice(block);
    }
    modified.extend_from_slice(&data[insert_pos..]);
    modified
}

/// An Abstract Syntax Block (a CBOR sequence, not an array) with an
/// unrecognised security context, one target, and one empty result set.
pub fn make_unknown_context_asb(target: u64) -> Vec<u8> {
    let mut encoder = Encoder::new();
    encoder.emit_array(Some(1), |a| {
        a.emit(&target);
    });
    encoder.emit(&99u64); // unrecognised context id
    encoder.emit(&0u64); // flags: no context parameters
    encoder.emit(&"ipn:1.0".parse::<Eid>().unwrap()); // security source
    encoder.emit_array(Some(1), |a| {
        a.emit_array(Some(0), |_| {}); // empty result set for the target
    });
    encoder.build()
}
