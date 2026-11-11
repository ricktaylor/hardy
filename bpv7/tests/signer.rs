//! Integration tests for the BPSec signing/integrity API.
//!
//! These exercise the public `Signer` + `Editor` surface, so they require a
//! security context (`rfc9173`, which enables `bpsec`) and `serde` for the JWK
//! key literals. The whole file is gated so `--no-default-features` builds do
//! not attempt to reference the feature-gated modules.
#![cfg(all(feature = "rfc9173", feature = "serde"))]

use hardy_bpv7::{
    Bundle, block,
    bpsec::{self, edit::BPSecEditor, key, rfc9173::ScopeFlags, signer},
    builder::Builder,
    crc,
    creation_timestamp::CreationTimestamp,
    editor::Editor,
    parse,
};
use std::collections::HashMap;

mod common;
use self::common::rand_k;
// Signer works on a parse-shaped `Bundle`; re-parse the builder output to
// get one with real wire extents.
fn reparse(
    bytes: &[u8],
) -> (
    ::bytes::Bytes,
    Bundle,
    HashMap<u64, bpsec::bib::OperationSet>,
) {
    let parse::Parsed {
        data, bundle, bibs, ..
    } = parse::parse(::bytes::Bytes::copy_from_slice(bytes)).expect("Failed to parse");
    (data, bundle, bibs)
}

// Signing the primary block leaves its CRC as it stands, the primary being
// immutable (RFC 9171 §4.3.1), and the signature verifies over the primary
// exactly as received: its bytes are unchanged.
#[test]
fn sign_primary_keeps_its_crc_and_verifies() {
    let (_bundle, bundle_bytes) =
        Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_payload(b"sign-primary test".as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();
    let (_, raw, _) = reparse(&bundle_bytes);
    assert_ne!(
        raw.blocks[&0].crc_type,
        crc::CrcType::None,
        "primary starts with a CRC"
    );

    let key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "HS256",
        "key_ops": ["sign", "verify"],
        "k": rand_k(18)
    }))
    .unwrap();
    let keys = key::KeySet::new(vec![key.clone()]);

    let mut signer = signer::Signer::new(&raw, &bundle_bytes);
    signer
        .sign_block(
            0,
            signer::Context::HMAC_SHA2(ScopeFlags::default()),
            "ipn:2.1".parse().unwrap(),
            &key,
        )
        .expect("Failed to sign");
    let signed_bytes = signer.rebuild().expect("Failed to rebuild");

    // Round-trip: the signature must verify.
    let (data, signed, _) = reparse(&signed_bytes);
    common::verify_all_bibs(&data, &keys).expect("signed primary bundle must verify");

    assert_eq!(
        signed.blocks[&0].crc_type, raw.blocks[&0].crc_type,
        "signing the primary keeps its CRC"
    );
    assert_eq!(
        signed.blocks[&0].payload(&data),
        raw.blocks[&0].payload(&bundle_bytes),
        "the primary's bytes are unchanged"
    );
    assert!(matches!(signed.blocks[&0].bib, block::BibCoverage::Some(_)));
}

// remove_integrity must clear the target's BIB coverage in the rebuilt Bundle,
// not leave a dangling reference to the removed BIB.
#[test]
fn remove_integrity_clears_target_coverage() {
    let (_bundle, bundle_bytes) =
        Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_payload(b"remove-integrity coverage test".as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();
    let (_, raw, _) = reparse(&bundle_bytes);

    let key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "HS256",
        "key_ops": ["sign", "verify"],
        "k": rand_k(18)
    }))
    .unwrap();
    let keys = key::KeySet::new(vec![key.clone()]);

    let mut signer = signer::Signer::new(&raw, &bundle_bytes);
    signer
        .sign_block(
            1,
            signer::Context::HMAC_SHA2(ScopeFlags::default()),
            "ipn:2.1".parse().unwrap(),
            &key,
        )
        .expect("Failed to sign");
    // Sign the payload block, then re-parse and verify the signature.
    let signed_bytes = signer.rebuild().expect("Failed to rebuild");

    let (data, signed, _) = reparse(&signed_bytes);
    common::verify_all_bibs(&data, &keys).expect("Failed to verify signed bundle");
    assert!(
        matches!(signed.blocks[&1].bib, block::BibCoverage::Some(_)),
        "payload block should report BIB coverage after signing"
    );
    // Default scope flags include the primary block in the IPPT: the primary
    // keeps its CRC, included as it stands (§3.7), as every block does.
    assert_ne!(
        signed.blocks[&0].crc_type,
        crc::CrcType::None,
        "the primary keeps its CRC"
    );

    let mut editor = Editor::new(&signed, &data);
    editor
        .remove_integrity(1)
        .expect("Failed to remove integrity");
    // remove_integrity() takes the *target* block number, not the BIB's.
    let (rebuilt, _chunks) = editor.rebuild_bundle().expect("Failed to rebuild bundle");

    assert_eq!(
        rebuilt.blocks[&1].bib,
        block::BibCoverage::None,
        "coverage must be cleared after remove_integrity"
    );
    assert!(
        !rebuilt
            .blocks
            .values()
            .any(|b| b.block_type == block::Type::BlockIntegrity),
        "no BIB block should remain after remove_integrity"
    );
}
