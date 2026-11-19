//! Shared fixtures for the bpv7 integration tests: byte-splicing helpers
//! for hand-crafting malformed wire input, plus a fresh-key generator for
//! JWK fixtures. Lives under `tests/common/` so cargo treats it as a
//! module, not its own test binary.
//!
//! Different test binaries use different subsets, so unused helpers per
//! binary are expected.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};

use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};
use hardy_bpv7::{
    block,
    bpsec::{
        self, Role, Roles, SecurityService, SecurityServices,
        checks::{self, Checked},
        edit::BPSecEditor,
        key::{KeySet, KeySource},
    },
    editor::{self, Chunk, Editor},
    eid::Eid,
    parse,
};
use hardy_cbor::{
    decode::skip_value,
    encode::{Bytes, Encoder, emit_array},
};
use rand::{TryRng, rngs::SysRng};

// A generous CBOR nesting bound for skipping a primary block: a flat
// array whose deepest members are EID and timestamp arrays.
const PRIMARY_BLOCK_SKIP_DEPTH: usize = 16;

// A canonical block `[type, number, flags, crc_type=none, data]`.
pub fn make_block(block_type: u64, block_number: u64, flags: u64, payload: &[u8]) -> Vec<u8> {
    emit_array(Some(5), |a| {
        a.emit(&block_type);
        a.emit(&block_number);
        a.emit(&flags);
        a.emit(&0u64); // CRC type: none
        a.emit(&Bytes(payload));
    })
}

// Byte offset one past the primary block of a serialised bundle.
pub fn end_of_primary(data: &[u8]) -> usize {
    assert_eq!(
        data[0], 0x9F,
        "bundle should start with an indefinite array"
    );
    let (_, primary_len) =
        skip_value(&data[1..], PRIMARY_BLOCK_SKIP_DEPTH).expect("should skip the primary block");
    1 + primary_len
}

// Splice raw block bytes in between the primary block and the payload block.
pub fn insert_after_primary(data: &[u8], blocks: &[&[u8]]) -> Vec<u8> {
    let insert_pos = end_of_primary(data);

    let mut modified = data[..insert_pos].to_vec();
    for block in blocks {
        modified.extend_from_slice(block);
    }
    modified.extend_from_slice(&data[insert_pos..]);
    modified
}

// An Abstract Syntax Block (a CBOR sequence, not an array) with an
// unrecognised security context, one target, and one empty result set.
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

// Base64url (no padding) of `len` freshly generated random bytes: a JWK
// `k` value for tests where the key's value is immaterial.
pub fn rand_k(len: usize) -> String {
    let mut buf = vec![0u8; len];
    SysRng.try_fill_bytes(&mut buf).unwrap();
    BASE64_URL_SAFE_NO_PAD.encode(buf)
}

// The bundle as its destination reads it: every block's encryption removed
// with `keys`, as `BPSecEditor::remove_encryption` removes it (with the
// encrypted BIB coupled to the block), and re-parsed, so every BIB is
// plaintext. Block numbers are kept. The encrypted BIBs go first, each with
// the targets coupled to it, since the parse reads a Previous Node or
// Bundle Age block, which no valid bundle has under an encrypted BIB, as
// covered by none. Fails on the first block the keys do not decrypt.
pub fn accept_all(parsed: &parse::Parsed, keys: &KeySet) -> Result<parse::Parsed, editor::Error> {
    let (bibs, targets): (Vec<u64>, Vec<u64>) = parsed
        .bundle
        .blocks
        .iter()
        .filter(|(_, b)| b.bcb.is_some())
        .map(|(&n, _)| n)
        .partition(|n| parsed.bundle.blocks[n].block_type == block::Type::BlockIntegrity);
    let mut editor = Editor::new(&parsed.bundle, &parsed.data).with_key_source(keys);
    for n in bibs.into_iter().chain(targets) {
        match editor.remove_encryption(n) {
            // Taken with an earlier block's coupled BIB.
            Ok(()) | Err(editor::Error::NotEncrypted) => {}
            Err(e) => return Err(e),
        }
    }
    let data = Chunk::flatten_bytes(editor.rebuild()?, parsed.data.clone());
    Ok(parse::parse(data)?)
}

// Every BIB operation in `data`, a complete bundle, checked with `keys` by
// the keyed verification over a fresh parse: an error for a failed check,
// as a strict caller wants, and a panic should anything be deferred.
pub fn verify_all_bibs(data: &[u8], keys: &KeySet) -> Result<(), hardy_bpv7::Error> {
    let facts = parse::parse(::bytes::Bytes::copy_from_slice(data))?.check(
        keys,
        &HashMap::new(),
        Roles::each(Role::Verifier, SecurityServices::INTEGRITY),
    )?;
    if facts
        .targets_of(SecurityService::Integrity, Checked::Failed)
        .next()
        .is_some()
    {
        return Err(bpsec::Error::IntegrityCheckFailed.into());
    }
    assert!(
        !facts.any(Checked::Deferred),
        "a complete buffer defers nothing"
    );
    Ok(())
}

// The per-hop blocks a receiving node accepts (RFC 9171 §5.4): every
// Previous Node, Bundle Age and Hop Count block.
pub fn per_hop_blocks(blocks: &HashMap<u64, block::Block>) -> HashSet<u64> {
    blocks
        .iter()
        .filter(|(_, b)| {
            matches!(
                b.block_type,
                block::Type::PreviousNode | block::Type::BundleAge | block::Type::HopCount
            )
        })
        .map(|(&n, _)| n)
        .collect()
}

// The keyed pass a receiving node makes over `parsed` (`Parsed::check`): the
// per-hop blocks and `acceptors` accepted, both services, and every other
// readable BIB operation verified, one `Facts`.
pub fn verify_as_receiver(
    parsed: &mut parse::Parsed,
    keys: &dyn KeySource,
    acceptors: &HashSet<u64>,
) -> Result<checks::Facts, hardy_bpv7::Error> {
    let mut accepted = per_hop_blocks(&parsed.bundle.blocks);
    accepted.extend(acceptors.iter().copied());
    let roles: HashMap<u64, Roles> = accepted
        .into_iter()
        .map(|n| (n, Roles::each(Role::Acceptor, SecurityServices::BOTH)))
        .collect();
    parsed.check(
        keys,
        &roles,
        Roles::each(Role::Verifier, SecurityServices::INTEGRITY),
    )
}
