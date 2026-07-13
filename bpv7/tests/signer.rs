//! Integration tests for the BPSec signing/integrity API.
//!
//! These exercise the public `Signer` + `Editor` surface, so they require a
//! security context (`rfc9173`, which enables `bpsec`) and `serde` for the JWK
//! key literals. The whole file is gated so `--no-default-features` builds do
//! not attempt to reference the feature-gated modules.
#![cfg(all(feature = "rfc9173", feature = "serde"))]

use hardy_bpv7::{
    block,
    bpsec::{key, rfc9173::ScopeFlags, signer},
    builder::Builder,
    bundle, crc,
    creation_timestamp::CreationTimestamp,
    editor::Editor,
};

// Signing the primary block must remove its CRC before generating the IPPT
// (RFC 9173 §3.8.1 — the target's CRC MUST be removed; no primary exemption),
// and the resulting signature must verify (sign and verify compute the IPPT over
// the same CRC-removed canonical primary).
#[test]
fn sign_primary_removes_crc_and_verifies() {
    let (bundle, bundle_bytes) =
        Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_payload(b"sign-primary test".as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();
    assert_ne!(
        bundle.blocks[&0].crc_type,
        crc::CrcType::None,
        "primary starts with a CRC"
    );

    let key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "HS256",
        "key_ops": ["sign", "verify"],
        "k": "c2VjcmV0X3NpZ25pbmdfa2V5"
    }))
    .unwrap();
    let keys = key::KeySet::new(vec![key.clone()]);

    let signed_bytes = signer::Signer::new(&bundle, &bundle_bytes)
        .sign_block(
            0,
            signer::Context::HMAC_SHA2(ScopeFlags::default()),
            "ipn:2.1".parse().unwrap(),
            &key,
        )
        .map_err(|(_, e)| e)
        .expect("Failed to sign")
        .rebuild()
        .expect("Failed to rebuild");

    // Round-trip: the signature must verify.
    let parsed = bundle::ParsedBundle::parse_with_keys(&signed_bytes, &keys)
        .expect("signed primary bundle must verify");

    // §3.8.1: the primary's CRC must have been removed before signing.
    assert_eq!(
        parsed.bundle.blocks[&0].crc_type,
        crc::CrcType::None,
        "signing the primary must remove its CRC (RFC 9173 §3.8.1)"
    );
    assert!(matches!(
        parsed.bundle.blocks[&0].bib,
        block::BibCoverage::Some(_)
    ));
}

// remove_integrity must clear the target's BIB coverage in the rebuilt Bundle,
// not leave a dangling reference to the removed BIB.
#[test]
fn remove_integrity_clears_target_coverage() {
    let (bundle, bundle_bytes) =
        Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_payload(b"remove-integrity coverage test".as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();

    let key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "HS256",
        "key_ops": ["sign", "verify"],
        "k": "c2VjcmV0X3NpZ25pbmdfa2V5"
    }))
    .unwrap();
    let keys = key::KeySet::new(vec![key.clone()]);

    // Sign the payload block, then re-parse (verifies the signature).
    let signed_bytes = signer::Signer::new(&bundle, &bundle_bytes)
        .sign_block(
            1,
            signer::Context::HMAC_SHA2(ScopeFlags::default()),
            "ipn:2.1".parse().unwrap(),
            &key,
        )
        .map_err(|(_, e)| e)
        .expect("Failed to sign")
        .rebuild()
        .expect("Failed to rebuild");

    let parsed = bundle::ParsedBundle::parse_with_keys(&signed_bytes, &keys)
        .expect("Failed to parse signed bundle");
    assert!(
        matches!(parsed.bundle.blocks[&1].bib, block::BibCoverage::Some(_)),
        "payload block should report BIB coverage after signing"
    );
    // Default scope flags include the primary block in the IPPT, but the primary
    // is not the BIB target here — RFC 9173 §3.8.1 removes only the *target's*
    // CRC, so the primary retains its CRC (included as-is per §3.7 step 2).
    assert_ne!(
        parsed.bundle.blocks[&0].crc_type,
        crc::CrcType::None,
        "primary is IPPT scope context, not a target, so its CRC is retained"
    );

    // remove_integrity() takes the *target* block number, not the BIB's.
    let (rebuilt, _chunks) = Editor::new(&parsed.bundle, &signed_bytes)
        .remove_integrity(1)
        .map_err(|(_, e)| e)
        .expect("Failed to remove integrity")
        .rebuild_bundle()
        .expect("Failed to rebuild bundle");

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
