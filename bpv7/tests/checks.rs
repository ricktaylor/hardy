//! Integration tests for the BPSec validation/rewrite pipeline — composing
//! the public `hardy_bpv7::{parse, checks, rewrite}` primitives the way a
//! consumer would (mirrors the reference pipeline in `bpa::bundle::parse`).

use bytes::Bytes;
use hardy_bpv7::parse::Parsed;
use hardy_bpv7::{
    Bundle, Error, block, bpsec, builder, checks, creation_timestamp, editor, eid, parse, rewrite,
};
use std::collections::{HashMap, HashSet};

/// Adapter: drive the public `parse::parse` and expose the legacy 4-tuple
/// shape the pipeline tests are written against.
#[allow(clippy::type_complexity)]
fn raw_parse_tuple(
    data: Bytes,
) -> Result<
    (
        Bytes,
        Bundle,
        HashMap<u64, bpsec::bcb::OperationSet>,
        HashMap<u64, bpsec::bib::OperationSet>,
    ),
    Error,
> {
    let Parsed {
        data,
        bundle,
        bcbs,
        bibs,
    } = parse::parse(data)?;
    // Every parsed vector doubles as an `encoded_len` conformance check:
    // the derivation must equal the wire length (RFC 9171 §4.1 —
    // payload block last, one closing break byte).
    assert_eq!(
        bundle.encoded_len(),
        data.len() as u64,
        "encoded_len must match the wire length"
    );
    Ok((data, bundle, bcbs, bibs))
}

fn empty_keys() -> bpsec::key::KeySet {
    bpsec::key::KeySet::new(vec![])
}

// Full-mode pipeline: composes the per-section helpers exactly as
// `bpa::bundle::parse` does, so the cascade tests exercise the real
// composition. Returns the (possibly rewritten) bundle plus the chunk
// plan when rewrites were applied.
#[allow(clippy::result_large_err)]
fn parse_full_for_test(
    data: &[u8],
    keys: &bpsec::key::KeySet,
) -> Result<(Bundle, Option<Vec<editor::Chunk>>), Error> {
    let bytes = Bytes::copy_from_slice(data);
    let (data, mut raw, bcb_ops, mut bib_ops) = raw_parse_tuple(bytes)?;

    let classification = checks::classify_unsupported(&raw.blocks, &bcb_ops, &bib_ops, &[])?;
    let mut to_remove: HashSet<u64> = HashSet::new();
    to_remove.extend(classification.unrecognised_deletable);
    for n in &classification.bib_deletable {
        to_remove.insert(*n);
        bib_ops.remove(n);
    }

    let mut decrypted = HashMap::new();
    let to_update: HashMap<u64, Vec<u8>> = HashMap::new();
    let facts = checks::verify(
        &data,
        keys,
        &mut raw.blocks,
        &bcb_ops,
        &mut bib_ops,
        &mut decrypted,
        &to_update,
    )?;
    // RFC 9172 §5.1.1: corrupt payload → discard bundle; corrupt
    // non-payload → remove the target only. The editor cascade strips it
    // from its covering BCB and drops the BCB once it empties; naming the
    // BCB here would strand a surviving co-target's ciphertext.
    for &target in &facts.failed {
        if target == 1 {
            return Err(bpsec::Error::DecryptionFailed.into());
        }
        to_remove.insert(target);
    }
    // The NoKey liveness policy (fatal for encrypted HopCount / unclocked
    // BundleAge) is exercised where it lives — the bpa ingress path — not
    // mirrored here; the test style guide forbids re-implementing the
    // algorithm under test, and no fixture in this file produces a
    // non-empty `facts.nokey_ext`.

    // §D (extension-field extraction / canonical re-emit) is a BPA concern
    // and now lives in `hardy-bpa`; the cascade-test bundles carry no
    // PreviousNode/HopCount, so there are no canonical re-emits to queue here.

    let chunks = if to_update.is_empty() && to_remove.is_empty() {
        None
    } else {
        rewrite::apply_rewrites(&data, &raw, keys, to_update, to_remove)?.map(
            |(new_raw, chunks)| {
                raw = new_raw;
                chunks
            },
        )
    };

    Ok((raw, chunks))
}

// Build a minimal valid bundle and return its serialised bytes.
fn build_minimal_bundle() -> Box<[u8]> {
    builder::Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
        .with_payload("Hello".as_bytes().into())
        .build(creation_timestamp::CreationTimestamp::now())
        .unwrap()
        .1
}

// Requirement: LLR 1.1.25 — roundtrip: build → serialise → parse → verify
#[test]
fn build_parse_roundtrip() {
    let src: eid::Eid = "ipn:1.0".parse().unwrap();
    let dst: eid::Eid = "ipn:2.0".parse().unwrap();
    let (original, data) = builder::Builder::new(src.clone(), dst.clone())
        .with_payload("Roundtrip".as_bytes().into())
        .build(creation_timestamp::CreationTimestamp::now())
        .unwrap();

    // Verify canonicalization-mode invariants by composing primitives
    // directly: callers run `parse::parse` plus the per-section helpers they
    // actually care about.
    let (_data, raw_bundle, bcb_ops, bib_ops) =
        raw_parse_tuple(Bytes::copy_from_slice(&data)).unwrap();
    assert_eq!(raw_bundle.primary.id.source, original.primary.id.source);
    assert_eq!(raw_bundle.primary.destination, original.primary.destination);
    assert_eq!(raw_bundle.primary.report_to, original.primary.report_to);
    assert_eq!(raw_bundle.primary.lifetime, original.primary.lifetime);
    assert!(bcb_ops.is_empty(), "Builder output has no BCBs");
    assert!(bib_ops.is_empty(), "Builder output has no BIBs");
    // Full mode would additionally classify unrecognised/unsupported
    // blocks. Builder output has none, so no deletables either.
    let classification =
        checks::classify_unsupported(&raw_bundle.blocks, &bcb_ops, &bib_ops, &[]).unwrap();
    assert!(
        classification.unrecognised_deletable.is_empty(),
        "Builder has no unrecognised blocks"
    );
    assert!(
        classification.bib_deletable.is_empty(),
        "Builder has no unsupported BIBs"
    );
    assert!(
        !classification.report_unsupported_block && !classification.report_unsupported_security,
        "Builder has no unsupported blocks"
    );
}

// Requirement: LLR 1.1.30
#[test]
fn unknown_block_discard() {
    let data = build_minimal_bundle();

    // Insert an unknown extension block (type 999) with delete_block_on_failure flag
    // between the primary block and the payload block.
    let unknown_block = hardy_cbor::encode::emit_array(Some(5), |a| {
        a.emit(&999u64); // block type
        a.emit(&2u64); // block number
        a.emit(&0x10u64); // flags: delete_block_on_failure
        a.emit(&0u64); // CRC type: none
        a.emit(&hardy_cbor::encode::Bytes(&[0xDE, 0xAD, 0xBE, 0xEF]));
    });

    assert_eq!(data[0], 0x9F, "Bundle should start with indefinite array");

    let (_, primary_len) =
        hardy_cbor::decode::skip_value(&data[1..], 16).expect("Should skip primary block");

    let insert_pos = 1 + primary_len;
    let mut modified = Vec::with_capacity(data.len() + unknown_block.len());
    modified.extend_from_slice(&data[..insert_pos]);
    modified.extend_from_slice(&unknown_block);
    modified.extend_from_slice(&data[insert_pos..]);

    // Preserve-mode semantics demonstrated via primitives: parse keeps every
    // block, classify_unsupported identifies block 2 as deletable,
    // and a Preserve-mode caller ignores the deletable list (block 2 stays).
    let (modified, raw_bundle, bcb_ops, bib_ops) =
        raw_parse_tuple(Bytes::copy_from_slice(&modified))
            .expect("parse accepts the unknown block");
    assert!(
        raw_bundle.blocks.contains_key(&2),
        "parse should preserve unknown block 2"
    );
    let classification = checks::classify_unsupported(&raw_bundle.blocks, &bcb_ops, &bib_ops, &[])
        .expect("unknown block has no delete_bundle_on_failure flag");
    assert!(
        classification.unrecognised_deletable.contains(&2),
        "block 2 is marked deletable (delete_block_on_failure flag set) — \
         Preserve-mode callers ignore this list"
    );

    // Full-mode end-to-end smoke check via composed primitives — verifies
    // that the deletable list produced by classify_* actually flows through
    // apply_rewrites and the block is gone.
    let (bundle, _chunks) = parse_full_for_test(&modified, &empty_keys())
        .unwrap_or_else(|error| panic!("Bundle with unknown block should parse: {error}"));
    assert!(
        !bundle.blocks.contains_key(&2),
        "Full mode should have removed unknown block 2"
    );
    assert!(
        bundle.blocks.contains_key(&1),
        "Payload block should still be present"
    );
}

// Splice an extension block (already encoded as a 5-element block array)
// into `data` immediately after the primary block.
fn splice_after_primary(data: &[u8], block: &[u8]) -> Vec<u8> {
    assert_eq!(data[0], 0x9F, "Bundle should start with indefinite array");
    let (_, primary_len) =
        hardy_cbor::decode::skip_value(&data[1..], 16).expect("Should skip primary block");
    let insert_pos = 1 + primary_len;
    let mut modified = Vec::with_capacity(data.len() + block.len());
    modified.extend_from_slice(&data[..insert_pos]);
    modified.extend_from_slice(block);
    modified.extend_from_slice(&data[insert_pos..]);
    modified
}

// Splice a BCB carrying an unrecognised security context (id 99) targeting
// the payload (block 1) into `data` as block number 2, with the given block
// processing `flags`. `flags` must include must-replicate (0x01) — required
// for a payload-targeting BCB.
fn splice_unrecognised_bcb(data: &[u8], flags: u64) -> Vec<u8> {
    // ASB CBOR sequence: targets [1], context id 99, context flags 0 (no
    // parameters), source EID, then one result list per target.
    let result_val = [0x41u8, 0xAA]; // result value: bytes(0xAA)
    let mut asb = hardy_cbor::encode::emit(&[1u64]).0;
    asb.extend(hardy_cbor::encode::emit(&99u64).0);
    asb.extend(hardy_cbor::encode::emit(&0u64).0);
    asb.extend(hardy_cbor::encode::emit(&"ipn:3.0".parse::<eid::Eid>().unwrap()).0);
    asb.extend(hardy_cbor::encode::emit_array(Some(1), |results| {
        results.emit_array(Some(1), |target_results| {
            target_results.emit(&(1u64, hardy_cbor::encode::Raw(&result_val)));
        });
    }));

    let bcb_block = hardy_cbor::encode::emit_array(Some(5), |a| {
        a.emit(&12u64); // block type: BCB
        a.emit(&2u64); // block number
        a.emit(&flags);
        a.emit(&0u64); // CRC type: none
        a.emit(&hardy_cbor::encode::Bytes(&asb));
    });
    splice_after_primary(data, &bcb_block)
}

// Requirement: RFC 9172 §7.1 — the §A facts distinguish an unsupported
// security operation from an unrecognised plain block, so the caller can
// select between the RFC 9172 and RFC 9171 report reasons.
#[test]
fn classify_distinguishes_security_kind() {
    // Unrecognised-context BCB: must_replicate (payload target) +
    // report_on_failure (0x03) → only the security fact fires.
    let modified = splice_unrecognised_bcb(&build_minimal_bundle(), 0x03);
    let (_, raw_bundle, bcb_ops, bib_ops) = raw_parse_tuple(Bytes::copy_from_slice(&modified))
        .expect("parse accepts an unrecognised-context BCB");
    let classification =
        checks::classify_unsupported(&raw_bundle.blocks, &bcb_ops, &bib_ops, &[]).unwrap();
    assert!(classification.report_unsupported_security);
    assert!(!classification.report_unsupported_block);

    // Control: an unknown (non-security) block with report_on_failure (0x02)
    // → only the block fact fires.
    let unknown_block = hardy_cbor::encode::emit_array(Some(5), |a| {
        a.emit(&999u64); // block type
        a.emit(&2u64); // block number
        a.emit(&0x02u64); // flags: report_on_failure
        a.emit(&0u64); // CRC type: none
        a.emit(&hardy_cbor::encode::Bytes(&[0xDE, 0xAD]));
    });
    let modified = splice_after_primary(&build_minimal_bundle(), &unknown_block);
    let (_, raw_bundle, bcb_ops, bib_ops) =
        raw_parse_tuple(Bytes::copy_from_slice(&modified)).unwrap();
    let classification =
        checks::classify_unsupported(&raw_bundle.blocks, &bcb_ops, &bib_ops, &[]).unwrap();
    assert!(classification.report_unsupported_block);
    assert!(!classification.report_unsupported_security);
}

// Requirement: RFC 9172 §7.1 — a delete-bundle-on-failure block carrying an
// unsupported security operation surfaces the security block's own error
// (here `UnrecognisedContext`), not the plain-block `Unsupported`, so the
// caller can report `UnknownSecurityOperation` instead of `BlockUnsupported`.
#[test]
fn unsupported_security_delete_bundle_errors() {
    // must_replicate + delete_bundle_on_failure (0x05).
    let modified = splice_unrecognised_bcb(&build_minimal_bundle(), 0x05);
    let (_, raw_bundle, bcb_ops, bib_ops) =
        raw_parse_tuple(Bytes::copy_from_slice(&modified)).unwrap();
    assert!(matches!(
        checks::classify_unsupported(&raw_bundle.blocks, &bcb_ops, &bib_ops, &[]),
        Err(Error::InvalidBPSec(bpsec::Error::UnrecognisedContext(99)))
    ));
}

// End-to-end tests for the BCB-covered BIB re-encryption cascade through
// `parse_full_for_test` → `rewrite::apply_rewrites` →
// `bpsec::edit::BPSecEditor::remove_blocks` (which internally calls the
// private `reencrypt_covered_bib`). Requires rfc9173 (for BCB-AES-GCM +
// BIB-HMAC-SHA2) and serde (for JWK deserialisation).
#[cfg(all(feature = "rfc9173", feature = "serde"))]
mod cascade_reencryption_tests {
    use super::*;

    fn sign_key() -> bpsec::key::Key {
        serde_json::from_value(serde_json::json!({
            "kid": "ipn:2.1",
            "kty": "oct",
            "alg": "HS256",
            "key_ops": ["sign", "verify"],
            "k": "c2VjcmV0X3NpZ25pbmdfa2V5"
        }))
        .unwrap()
    }

    fn enc_key() -> bpsec::key::Key {
        serde_json::from_value(serde_json::json!({
            "kid": "ipn:2.1",
            "kty": "oct",
            "alg": "A128KW",
            "enc": "A128GCM",
            "key_ops": ["encrypt", "decrypt", "wrapKey", "unwrapKey"],
            "k": "AAAAAAAAAAAAAAAAAAAAAA"
        }))
        .unwrap()
    }

    // Hand-construct a bundle byte sequence with a payload plus an unknown
    // extension block (type 999, block #2, flagged delete_block_on_failure).
    // The unknown block is what the cascade later drops.
    fn build_with_unknown_block() -> Vec<u8> {
        let (_, base) =
            builder::Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
                .with_payload(b"payload data".as_slice().into())
                .build(creation_timestamp::CreationTimestamp::now())
                .unwrap();
        let unknown = hardy_cbor::encode::emit_array(Some(5), |a| {
            a.emit(&999u64);
            a.emit(&2u64);
            a.emit(&0x10u64); // delete_block_on_failure
            a.emit(&0u64); // CRC: none
            a.emit(&hardy_cbor::encode::Bytes(&[0xDE, 0xAD]));
        });
        assert_eq!(base[0], 0x9F);
        let (_, primary_len) =
            hardy_cbor::decode::skip_value(&base[1..], 16).expect("skip primary");
        let insert_pos = 1 + primary_len;
        let mut out = Vec::with_capacity(base.len() + unknown.len());
        out.extend_from_slice(&base[..insert_pos]);
        out.extend_from_slice(&unknown);
        out.extend_from_slice(&base[insert_pos..]);
        out
    }

    // Sign the named targets under a single BIB (HMAC-SHA2, default scope
    // flags, source ipn:2.1) and return the rebuilt bytes.
    fn sign(bundle_bytes: &[u8], targets: &[u64], key: &bpsec::key::Key) -> Box<[u8]> {
        let (bundle_bytes, raw, _, _) =
            raw_parse_tuple(Bytes::copy_from_slice(bundle_bytes)).expect("parse");
        let mut signer = bpsec::signer::Signer::new(&raw, &bundle_bytes);
        for &t in targets {
            signer = signer
                .sign_block(
                    t,
                    bpsec::signer::Context::HMAC_SHA2(bpsec::rfc9173::ScopeFlags::default()),
                    "ipn:2.1".parse().unwrap(),
                    key,
                )
                .map_err(|(_, e)| e)
                .unwrap();
        }
        signer.rebuild().unwrap()
    }

    // Encrypt `target` (the encryptor auto-encrypts its BIB and the BIB's
    // other targets); each AES-GCM op lands in its own BCB.
    fn encrypt(bundle_bytes: &[u8], target: u64, enc_k: &bpsec::key::Key) -> Box<[u8]> {
        let (bundle_bytes, raw, _, _) =
            raw_parse_tuple(Bytes::copy_from_slice(bundle_bytes)).expect("parse");
        let flags = bpsec::rfc9173::ScopeFlags {
            include_security_header: false,
            ..bpsec::rfc9173::ScopeFlags::default()
        };
        let encryptor = bpsec::encryptor::Encryptor::new(&raw, &bundle_bytes)
            .encrypt_block(
                target,
                bpsec::encryptor::Context::AES_GCM(flags),
                "ipn:2.1".parse().unwrap(),
                enc_k,
            )
            .map_err(|(_, e)| e)
            .unwrap();
        encryptor.rebuild().unwrap()
    }

    // Extract the AES-GCM IV from the BCB protecting `target`. Re-parses the
    // bundle structurally (no decryption needed — the BCB OperationSet is
    // plaintext).
    fn iv_protecting(bundle_bytes: &[u8], target: u64) -> Box<[u8]> {
        let (bundle_bytes, raw, _, _) =
            raw_parse_tuple(Bytes::copy_from_slice(bundle_bytes)).expect("parse");
        let bcb_num = raw
            .blocks
            .get(&target)
            .and_then(|b| b.bcb)
            .expect("target is BCB-encrypted");
        let bcb_block = raw.blocks.get(&bcb_num).expect("BCB block present");
        let bcb_payload = bcb_block
            .payload(&bundle_bytes)
            .expect("BCB body in bundle");
        let opset: bpsec::bcb::OperationSet =
            hardy_cbor::decode::parse(bcb_payload).expect("decode BCB");
        match opset.operations().get(&target).expect("BCB op for target") {
            bpsec::bcb::Operation::AES_GCM(op) => op.parameters.iv.clone(),
            bpsec::bcb::Operation::Unrecognised(..) => panic!("expected AES-GCM"),
        }
    }

    fn find_bib(bundle: &Bundle) -> Option<u64> {
        bundle
            .blocks
            .iter()
            .find_map(|(&n, b)| matches!(b.block_type, block::Type::BlockIntegrity).then_some(n))
    }

    fn count_type(bundle: &Bundle, ty: block::Type) -> usize {
        bundle
            .blocks
            .values()
            .filter(|b| b.block_type == ty)
            .count()
    }

    // Round-trip + fresh-IV regression + final-BIB-block-state.
    #[test]
    fn cascade_reencrypts_surviving_bib() {
        let sign_k = sign_key();
        let enc_k = enc_key();
        let keys = bpsec::key::KeySet::new(vec![sign_k.clone(), enc_k.clone()]);

        // Build → sign(payload + unknown) → encrypt(payload). The encryptor
        // auto-encrypts the BIB and the unknown block too; each AES-GCM op
        // gets its own BCB.
        let with_unknown = build_with_unknown_block();
        let signed = sign(&with_unknown, &[1, 2], &sign_k);
        let encrypted = encrypt(&signed, 1, &enc_k);

        // Sanity: BIB present, BIB itself BCB-encrypted, payload BIB-covered.
        let (parsed_bundle, _) = parse_full_for_test(&encrypted, &keys)
            .unwrap_or_else(|error| panic!("Pre-cascade inspect failed: {error}"));
        let bib_num = find_bib(&parsed_bundle).expect("BIB present");
        assert!(
            parsed_bundle.blocks[&bib_num].bcb.is_some(),
            "BIB must be BCB-encrypted (the case the helper handles)"
        );
        assert!(matches!(
            parsed_bundle.blocks[&1].bib,
            block::BibCoverage::Some(_)
        ));

        // Capture the pre-cascade IV of the BCB protecting the BIB.
        let old_iv = iv_protecting(&encrypted, bib_num);
        assert_eq!(old_iv.len(), 12, "AES-GCM IV is 12 bytes");

        // Run the cascade.
        let (new_bundle, new_data_chunks) = match parse_full_for_test(&encrypted, &keys) {
            Ok((bundle, Some(chunks))) => (bundle, chunks),
            Ok((_, None)) => {
                panic!("expected Rewritten — unknown block must trigger cascade")
            }
            Err(error) => panic!("Parse failed: {error}"),
        };
        let new_data = editor::Chunk::flatten(new_data_chunks, &encrypted);

        // Unknown block dropped; its orphaned BCB dropped too; BIB survives
        // with only the payload target left.
        assert!(!new_bundle.blocks.contains_key(&2), "unknown block dropped");
        assert_eq!(
            count_type(&new_bundle, block::Type::BlockSecurity),
            2,
            "BCB over unknown block must be orphaned and dropped"
        );
        let new_bib_num = find_bib(&new_bundle).expect("BIB survives (still covers payload)");
        assert!(
            new_bundle.blocks[&new_bib_num].bcb.is_some(),
            "Re-encrypted BIB still BCB-protected"
        );

        // Final BIB block state: wire bytes must be ciphertext, not the
        // plaintext OperationSet staged into the editor during the helper's
        // first update_block_inner pass.
        let new_bib_block = &new_bundle.blocks[&new_bib_num];
        let new_bib_wire = new_bib_block
            .payload(&new_data)
            .expect("BIB body in rewritten bundle");
        assert!(
            hardy_cbor::decode::parse::<bpsec::bib::OperationSet>(new_bib_wire).is_err(),
            "Re-encrypted BIB on the wire must NOT be a plaintext OperationSet — \
             staged plaintext leaked through to wire output"
        );

        // Fresh IV: AES-GCM key+IV reuse is catastrophic — verify the helper
        // produced a different IV than the original BCB.
        let new_iv = iv_protecting(&new_data, new_bib_num);
        assert_eq!(new_iv.len(), 12);
        assert_ne!(
            *new_iv, *old_iv,
            "Re-encrypted BCB must use a fresh IV (AES-GCM safety)"
        );

        // Round-trip: re-parsing the cascade output under the same keys must
        // succeed. `parse_full_for_test` internally runs `verify_all_bibs`,
        // so success here is the payload BIB authenticating after the cascade.
        let _ = parse_full_for_test(&new_data, &keys)
            .unwrap_or_else(|error| panic!("Re-parse after cascade failed: {error}"));
    }

    // When the dropped target leaves the BIB empty, the cascade drops the BIB
    // entirely — the re-encrypt helper must NOT be invoked. Verified by
    // setting up a single-target BIB and confirming the final bundle has no
    // BIB or BCB blocks at all.
    #[test]
    fn not_called_when_bib_empties() {
        let sign_k = sign_key();
        let enc_k = enc_key();
        let keys = bpsec::key::KeySet::new(vec![sign_k.clone(), enc_k.clone()]);

        let with_unknown = build_with_unknown_block();
        // Sign ONLY the unknown block → BIB has exactly one target.
        let signed = sign(&with_unknown, &[2], &sign_k);
        let encrypted = encrypt(&signed, 2, &enc_k);

        let new_bundle = match parse_full_for_test(&encrypted, &keys) {
            Ok((bundle, Some(_))) => bundle,
            Ok((_, None)) => panic!("expected Rewritten"),
            Err(error) => panic!("Parse failed: {error}"),
        };

        assert!(!new_bundle.blocks.contains_key(&2), "unknown block dropped");
        assert_eq!(
            count_type(&new_bundle, block::Type::BlockIntegrity),
            0,
            "BIB (emptied by cascade) must be dropped — helper must NOT be invoked"
        );
        assert_eq!(
            count_type(&new_bundle, block::Type::BlockSecurity),
            0,
            "All BCBs (orphaned by BIB drop) must be dropped"
        );
        assert!(new_bundle.blocks.contains_key(&1), "payload survives");
    }

    // RFC 9172 §5.1.1: when the BIB protecting an extension block is itself
    // BCB-encrypted with a wrong key (DecryptionFailed), the BIB and its BCB
    // are failure-dropped; the payload and bundle survive.
    #[test]
    fn corrupt_covered_bib_is_failure_dropped() {
        let sign_k = sign_key();
        let enc_k = enc_key();

        // Build a bundle where the payload BIB is BCB-encrypted.
        // sign(payload) → encrypt(payload) auto-encrypts the BIB covering it.
        let (_, base) =
            builder::Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
                .with_payload(b"payload data".as_slice().into())
                .build(creation_timestamp::CreationTimestamp::now())
                .unwrap();
        let signed = sign(&base, &[1], &sign_k);
        let encrypted = encrypt(&signed, 1, &enc_k);

        // Capture which block is the BIB and which BCB protects it (using
        // correct keys) so we can assert their removal precisely.
        let (pre, _) = parse_full_for_test(
            &encrypted,
            &bpsec::key::KeySet::new(vec![sign_k.clone(), enc_k.clone()]),
        )
        .expect("correct keys parse");
        let bib_num = find_bib(&pre).expect("BIB present");
        let bib_bcb_num = pre.blocks[&bib_num].bcb.expect("BIB must be BCB-encrypted");

        // A wrong enc key with the same kid → decrypt attempt produces
        // DecryptionFailed (not NoKey) at the §B BIB-decryption stage.
        let wrong_enc_k: bpsec::key::Key = serde_json::from_value(serde_json::json!({
            "kid": "ipn:2.1",
            "kty": "oct",
            "alg": "A128KW",
            "enc": "A128GCM",
            "key_ops": ["encrypt", "decrypt", "wrapKey", "unwrapKey"],
            "k": "AAAAAAAAAAAAAAAAAAAAAQ"
        }))
        .unwrap();
        let wrong_keys = bpsec::key::KeySet::new(vec![sign_k, wrong_enc_k]);

        // parse_full_for_test applies §5.1.1 failure-drop: the corrupt BIB
        // and the BCB that was protecting it are removed. The payload block
        // survives (it remains BCB-encrypted under its own separate BCB,
        // which is left intact — the payload itself is not corrupt).
        let (bundle, _chunks) = parse_full_for_test(&encrypted, &wrong_keys)
            .expect("§5.1.1 failure-drop: bundle survives a corrupt covered BIB");

        assert!(
            !bundle.blocks.contains_key(&bib_num),
            "corrupt BIB must be dropped"
        );
        assert!(
            !bundle.blocks.contains_key(&bib_bcb_num),
            "BCB protecting the corrupt BIB must be dropped"
        );
        assert!(bundle.blocks.contains_key(&1), "payload must survive");
    }

    // RFC 9172 §5.1.1 with a *shared* BCB — the RFC 9173 Appendix A.4 wire
    // vector, where one BCB (block 2) covers both the encrypted BIB (block
    // 3) and the payload (block 1). Corrupting the BIB's ciphertext must
    // failure-drop only the BIB: the cascade strips block 3 from the BCB's
    // OperationSet and the BCB survives covering the payload. Queuing the
    // shared BCB itself for removal would strand the payload ciphertext
    // (`StrandsCiphertext`) and panic `apply_rewrites`.
    #[test]
    fn multi_target_bcb_failure_drop_spares_the_shared_bcb() {
        // `hex_literal::hex!` is path-qualified rather than imported: a later
        // leg moves the file-level `hex!` users into tests/parse.rs and drops
        // the top-level `use`, but this test stays.
        let mut data = hex_literal::hex!(
            "9f88070000820282010282028202018202820201820018281a000f4240850b0300
             005846438ed6208eb1c1ffb94d952175167df0902902064a2983910c4fb2340790bf
             420a7d1921d5bf7c4721e02ab87a93ab1e0b75cf62e4948727c8b5dae46ed2af0543
             9b88029191850c0201005849820301020182028202018382014c5477656c76653132
             313231328202038204078281820150220ffc45c8a901999ecc60991dd78b29818201
             50d2c51cb2481792dae8b21d848cede99b8501010000582390eab6457593379298a8
             724e16e61f837488e127212b59ac91f8a86287b7d07630a122ff"
        )
        .to_vec();
        // Flip one byte of the BIB's ciphertext: the block-3 body is the
        // 70-byte string right after its `58 46` bytes header.
        let pos = data
            .windows(2)
            .position(|w| w == hex_literal::hex!("5846"))
            .expect("BIB body header present")
            + 2;
        data[pos] ^= 0x01;

        let keys: bpsec::key::KeySet = serde_json::from_value(serde_json::json!({
            "keys": [
                {
                    "kid": "ipn:2.1",
                    "kty": "oct",
                    "alg": "HS384",
                    "key_ops": ["verify"],
                    "k": "GisaKxorGisaKxorGisaKw"
                },
                {
                    "kid": "ipn:2.1",
                    "kty": "oct",
                    "enc": "A256GCM",
                    "key_ops": ["decrypt"],
                    "k": "cXdlcnR5dWlvcGFzZGZnaHF3ZXJ0eXVpb3Bhc2RmZ2g"
                }
            ]
        }))
        .unwrap();

        let (bundle, chunks) = parse_full_for_test(&data, &keys)
            .expect("§5.1.1 failure-drop: bundle survives a corrupt target of a shared BCB");
        assert!(chunks.is_some(), "the bundle must be rewritten");
        assert!(!bundle.blocks.contains_key(&3), "corrupt BIB dropped");
        assert!(
            bundle.blocks.contains_key(&2),
            "shared BCB survives, still covering the payload"
        );
        assert!(bundle.blocks.contains_key(&1), "payload survives");
    }

    // RFC 9172 §5.1.1: when remove_blocks is called on a target whose
    // covering BIB is BCB-encrypted with a wrong key, the cascade
    // leniently continues past the DecryptionFailed (instead of erroring)
    // and removes the target and all named security blocks cleanly.
    //
    // This exercises the edit-level failure-drop path directly — without
    // going through checks::verify — and confirms the DecryptFailed →
    // continue change in bpsec::edit::remove_blocks.
    #[test]
    fn remove_blocks_failure_drop_with_undecryptable_bib() {
        let sign_k = sign_key();
        let enc_k = enc_key();

        // sign([2]) → encrypt(2): the encryptor auto-encrypts the BIB
        // covering block 2, so the resulting bundle has:
        //   block 2 — unknown ext (BCB-encrypted, BibCoverage::Maybe)
        //   block 3 — BIB covering block 2 (BCB-encrypted)
        //   block 4 — BCB over block 2
        //   block 5 — BCB over BIB(3)
        let base = build_with_unknown_block();
        let signed = sign(&base, &[2], &sign_k);
        let encrypted = encrypt(&signed, 2, &enc_k);

        // Parse structurally to find block numbers (no keys needed).
        let (enc_bytes, raw, _, _) = raw_parse_tuple(Bytes::copy_from_slice(&encrypted)).unwrap();
        let bib_num = find_bib(&raw).expect("BIB present");
        let bcb_over_2 = raw.blocks[&2].bcb.expect("block 2 is BCB-encrypted");
        let bcb_over_bib = raw.blocks[&bib_num].bcb.expect("BIB is BCB-encrypted");

        // Wrong enc key (same kid, wrong bytes) → DecryptionFailed (not
        // NoKey) when remove_blocks tries to stage the BIB in step 2.
        let wrong_enc_k: bpsec::key::Key = serde_json::from_value(serde_json::json!({
            "kid": "ipn:2.1",
            "kty": "oct",
            "alg": "A128KW",
            "enc": "A128GCM",
            "key_ops": ["encrypt", "decrypt", "wrapKey", "unwrapKey"],
            "k": "AAAAAAAAAAAAAAAAAAAAAQ"
        }))
        .unwrap();
        let wrong_keys = bpsec::key::KeySet::new(vec![sign_k, wrong_enc_k]);

        // §5.1.1 failure-drop at the editor level: include the corrupt
        // block and all its associated security blocks in to_remove.
        // remove_blocks hits DecryptFailed on BIB staging and continues
        // (was a hard error before this fix); all four blocks are removed.
        let to_remove: HashSet<u64> = [2, bib_num, bcb_over_2, bcb_over_bib].into_iter().collect();
        let (bundle, _chunks) =
            rewrite::apply_rewrites(&enc_bytes, &raw, &wrong_keys, HashMap::new(), to_remove)
                .expect("apply_rewrites")
                .expect("at least one block was removed");

        assert!(
            !bundle.blocks.contains_key(&2),
            "corrupt block must be dropped"
        );
        assert!(!bundle.blocks.contains_key(&bib_num), "BIB must be dropped");
        assert!(
            !bundle.blocks.contains_key(&bcb_over_2),
            "BCB over corrupt block must be dropped"
        );
        assert!(
            !bundle.blocks.contains_key(&bcb_over_bib),
            "BCB over BIB must be dropped"
        );
        assert!(bundle.blocks.contains_key(&1), "payload must survive");
    }

    use hardy_bpv7::bpsec::edit::BPSecEditor;
    use hardy_bpv7::editor::Editor;

    // Removing a plaintext BIB outright must clear its surviving targets'
    // coverage stamps: the rebuilt Bundle must not report coverage by a
    // block that no longer exists (parse-review finding E3).
    #[test]
    fn removing_bib_outright_clears_target_coverage() {
        let sign_k = sign_key();
        let base = build_with_unknown_block();
        let signed = sign(&base, &[1, 2], &sign_k);

        let (bytes, raw, _, _) = raw_parse_tuple(Bytes::copy_from_slice(&signed)).unwrap();
        let bib_num = find_bib(&raw).expect("BIB present");
        assert!(matches!(raw.blocks[&1].bib, block::BibCoverage::Some(n) if n == bib_num));
        assert!(matches!(raw.blocks[&2].bib, block::BibCoverage::Some(n) if n == bib_num));

        let (editor, removed) = Editor::new(&raw, &bytes)
            .remove_blocks(HashSet::from([bib_num]), &empty_keys())
            .map_err(|(_, e)| e)
            .unwrap();
        assert_eq!(removed, HashSet::from([bib_num]));

        let (bundle, _) = editor.rebuild_bundle().unwrap();
        assert!(!bundle.blocks.contains_key(&bib_num), "BIB removed");
        assert!(
            matches!(bundle.blocks[&1].bib, block::BibCoverage::None),
            "target 1 coverage cleared"
        );
        assert!(
            matches!(bundle.blocks[&2].bib, block::BibCoverage::None),
            "target 2 coverage cleared"
        );
    }

    // remove_blocks screens its request: primary/payload are never
    // removable, and a BCB may only go together with all of its targets
    // (parse-review finding E5).
    #[test]
    fn remove_blocks_screens_request() {
        let sign_k = sign_key();
        let enc_k = enc_key();
        let base = build_with_unknown_block();

        // Plain bundle: primary and payload are refused outright.
        let (bytes, raw, _, _) = raw_parse_tuple(Bytes::copy_from_slice(&base)).unwrap();
        let (_, err) = Editor::new(&raw, &bytes)
            .remove_blocks(HashSet::from([0u64]), &empty_keys())
            .map(|_| ())
            .unwrap_err();
        assert!(matches!(err, editor::Error::PrimaryBlock), "{err}");
        let (_, err) = Editor::new(&raw, &bytes)
            .remove_blocks(HashSet::from([1u64]), &empty_keys())
            .map(|_| ())
            .unwrap_err();
        assert!(matches!(err, editor::Error::PayloadBlock), "{err}");

        // Encrypted bundle: removing the BCB alone would strand block 2's
        // ciphertext — refused. (Together with its target it succeeds, as
        // the failure-drop test above shows.)
        let signed = sign(&base, &[2], &sign_k);
        let encrypted = encrypt(&signed, 2, &enc_k);
        let (bytes, raw, _, _) = raw_parse_tuple(Bytes::copy_from_slice(&encrypted)).unwrap();
        let bcb_over_2 = raw.blocks[&2].bcb.expect("block 2 is BCB-encrypted");
        let (_, err) = Editor::new(&raw, &bytes)
            .remove_blocks(HashSet::from([bcb_over_2]), &empty_keys())
            .map(|_| ())
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("without also removing all of its targets"),
            "{err}"
        );
    }

    // A `Maybe`-covered target must not be removed while its covering
    // encrypted BIB survives undecrypted: the request is pulled back and
    // the bundle left untouched (parse-review finding E2 residual).
    #[test]
    fn maybe_covered_target_pulled_back_without_keys() {
        let sign_k = sign_key();
        let enc_k = enc_key();
        let base = build_with_unknown_block();
        let signed = sign(&base, &[2], &sign_k);
        let encrypted = encrypt(&signed, 2, &enc_k);

        let (bytes, raw, _, _) = raw_parse_tuple(Bytes::copy_from_slice(&encrypted)).unwrap();
        let bib_num = find_bib(&raw).expect("BIB present");
        assert!(matches!(raw.blocks[&2].bib, block::BibCoverage::Maybe));

        // No keys: the BIB stays opaque, and block 2 must be retained.
        let (editor, removed) = Editor::new(&raw, &bytes)
            .remove_blocks(HashSet::from([2u64]), &empty_keys())
            .map_err(|(_, e)| e)
            .unwrap();
        assert!(removed.is_empty(), "pulled-back request removes nothing");
        let (bundle, _) = editor.rebuild_bundle().unwrap();
        assert!(
            bundle.blocks.contains_key(&2),
            "Maybe-covered target retained"
        );
        assert!(
            bundle.blocks.contains_key(&bib_num),
            "encrypted BIB retained"
        );

        // And apply_rewrites maps the all-pulled-back request to "no
        // rewrite" (was: Rewritten with a byte-identical bundle).
        let result = rewrite::apply_rewrites(
            &bytes,
            &raw,
            &empty_keys(),
            HashMap::new(),
            HashSet::from([2u64]),
        )
        .expect("apply_rewrites");
        assert!(
            result.is_none(),
            "no rewrite for a fully pulled-back request"
        );
    }

    // Phantom block numbers in the request are not reported as removals
    // and produce no rewrite (parse-review finding E11e).
    #[test]
    fn phantom_removals_are_not_reported() {
        let base = build_with_unknown_block();
        let (bytes, raw, _, _) = raw_parse_tuple(Bytes::copy_from_slice(&base)).unwrap();

        let (_, removed) = Editor::new(&raw, &bytes)
            .remove_blocks(HashSet::from([99u64]), &empty_keys())
            .map_err(|(_, e)| e)
            .unwrap();
        assert!(removed.is_empty());

        let result = rewrite::apply_rewrites(
            &bytes,
            &raw,
            &empty_keys(),
            HashMap::new(),
            HashSet::from([99u64]),
        )
        .expect("apply_rewrites");
        assert!(
            result.is_none(),
            "phantom request must not read as Rewritten"
        );
    }

    // remove_encryption must clear the decrypted target's BCB coverage in
    // the rebuilt Bundle (parse-review finding E4).
    #[test]
    fn remove_encryption_clears_bcb_coverage() {
        let enc_k = enc_key();
        let keys = bpsec::key::KeySet::new(vec![enc_k.clone()]);

        let (_, base) =
            builder::Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
                .with_payload(b"payload data".as_slice().into())
                .build(creation_timestamp::CreationTimestamp::now())
                .unwrap();
        let encrypted = encrypt(&base, 1, &enc_k);
        let (bytes, raw, _, _) = raw_parse_tuple(Bytes::copy_from_slice(&encrypted)).unwrap();
        let bcb_num = raw.blocks[&1].bcb.expect("payload is BCB-encrypted");

        let editor = bpsec::edit::remove_encryption(Editor::new(&raw, &bytes), 1, &keys)
            .map_err(|(_, e)| e)
            .unwrap();
        let (bundle, _) = editor.rebuild_bundle().unwrap();
        assert!(
            !bundle.blocks.contains_key(&bcb_num),
            "single-target BCB dropped"
        );
        assert!(
            bundle.blocks[&1].bcb.is_none(),
            "target's BCB coverage cleared"
        );
    }

    // The producers (Signer/Encryptor) must only ever emit bundles that the
    // validator's structural rules (`bib`/`bcb::OperationSet::check`, run inside
    // `parse::parse`) accept. This is currently guaranteed by construction; this
    // test makes the invariant explicit and is the place to extend when a new
    // security context enables multi-target (shared) BCBs.
    #[test]
    fn producer_output_satisfies_structural_check() {
        let (_, base) =
            builder::Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
                .with_payload(b"payload data".as_slice().into())
                .build(creation_timestamp::CreationTimestamp::now())
                .unwrap();

        // Signer output -> bib::OperationSet::check (plaintext BIB) inside parse.
        let signed = sign(&base, &[1], &sign_key());
        parse::parse(Bytes::copy_from_slice(&signed))
            .expect("signer output must pass structural check");

        // Encryptor output -> bcb::OperationSet::check inside parse. Encrypting
        // the signed payload also encrypts its BIB (sign-before-encrypt).
        let encrypted = encrypt(&signed, 1, &enc_key());
        parse::parse(Bytes::copy_from_slice(&encrypted))
            .expect("encryptor output must pass structural check");
    }
}

// Deferred block-1 (payload) BIB verification — the streaming ingress gate path.
// On a headers-only buffer (oversized payload not yet drained), `verify` can't
// check a BIB that targets the payload, so it drains that op-set out of
// `bib_ops` and hands it over owned in `deferred_bibs`; the gate re-checks the
// handed-over map with `begin_payload_verification`, feeding each verifier the
// payload as it streams.
#[cfg(all(feature = "rfc9173", feature = "serde"))]
mod deferred_payload_bib_tests {
    use super::*;
    use hardy_bpv7::parse::{BundleParser, ParserProgress};

    fn sign_key() -> bpsec::key::Key {
        serde_json::from_value(serde_json::json!({
            "kid": "ipn:2.1",
            "kty": "oct",
            "alg": "HS256",
            "key_ops": ["sign", "verify"],
            "k": "c2VjcmV0X3NpZ25pbmdfa2V5"
        }))
        .unwrap()
    }

    fn keys() -> bpsec::key::KeySet {
        bpsec::key::KeySet::new(vec![sign_key()])
    }

    // A bundle whose payload (block 1) is signed under a BIB and is far larger
    // than any sane parser chunk, so the streaming parser must report `Partial`
    // before the payload body is resident.
    fn signed_large_payload() -> Box<[u8]> {
        let (_, base) =
            builder::Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
                .with_payload(vec![0xAB_u8; 50_000].as_slice().into())
                .build(creation_timestamp::CreationTimestamp::now())
                .unwrap();
        let (bytes, raw, _, _) = raw_parse_tuple(Bytes::copy_from_slice(&base)).expect("parse");
        bpsec::signer::Signer::new(&raw, &bytes)
            .sign_block(
                1,
                bpsec::signer::Context::HMAC_SHA2(bpsec::rfc9173::ScopeFlags::default()),
                "ipn:2.1".parse().unwrap(),
                &sign_key(),
            )
            .map_err(|(_, e)| e)
            .unwrap()
            .rebuild()
            .unwrap()
    }

    // Drive the streaming parser until the payload body overflows the buffer,
    // returning the parsed headers — block 1's extent over-claims, its body
    // isn't resident in `parsed.data`.
    fn parse_headers_only(full: &[u8]) -> Parsed {
        let mut parser = BundleParser::new(256);
        for c in full.chunks(64) {
            match parser.push(Bytes::copy_from_slice(c)).unwrap() {
                ParserProgress::NeedMore(_) => {}
                ParserProgress::Partial { consumed, .. } => {
                    return parser.finish(consumed).unwrap();
                }
                ParserProgress::Ready(_) => panic!("oversized payload must Partial, not Ready"),
            }
        }
        panic!("parser never reached Partial");
    }

    // Header pass defers the block-1 BIB (payload not resident), handing its
    // op-set over owned; the map then verifies against the full bundle.
    #[test]
    fn payload_bib_deferred_then_verified() {
        let full = signed_large_payload();
        let keys = keys();

        let Parsed {
            data: consumed,
            bundle: mut raw,
            bcbs: bcb_ops,
            bibs: mut bib_ops,
        } = parse_headers_only(&full);
        let bib_block = *bib_ops.keys().next().expect("a BIB op-set");
        assert!(
            raw.blocks.get(&1).unwrap().payload(&consumed).is_none(),
            "payload body must not be resident in the headers-only buffer"
        );

        let mut decrypted = HashMap::new();
        let no_updates = HashMap::new();
        let facts = checks::verify(
            &consumed,
            &keys,
            &mut raw.blocks,
            &bcb_ops,
            &mut bib_ops,
            &mut decrypted,
            &no_updates,
        )
        .unwrap();
        assert!(
            facts.deferred_bibs.contains_key(&bib_block) && facts.deferred_bibs.len() == 1,
            "the block-1 BIB is deferred, not checked inline"
        );
        assert!(
            !bib_ops.contains_key(&bib_block),
            "verify hands the deferred op-set over owned — drained out of bib_ops"
        );

        // The gate feeds each deferred verifier the now-resident payload and
        // settles it — the streaming drain does the same, segment by segment.
        // (A tampered-payload failure is the `_incremental_tamper_fails` twin
        // below.)
        for (_, mut verifier) in
            checks::begin_payload_verification(&full, &keys, &raw.blocks, &facts.deferred_bibs)
                .expect("verifier construction needs only header material")
        {
            let payload = raw.blocks.get(&1).unwrap().payload(&full).unwrap();
            verifier.update(payload);
            verifier
                .finish()
                .expect("deferred payload BIB verifies against the full bundle");
        }
    }

    // Run the header pass on the headers-only buffer and hand back the
    // pieces the streaming-verifier tests need: the consumed prefix, the
    // structural bundle, and the deferred op-set map.
    fn deferred_setup(
        full: &[u8],
        keys: &bpsec::key::KeySet,
    ) -> (Bytes, hardy_bpv7::bundle::Bundle, checks::VerifyFacts) {
        let Parsed {
            data: consumed,
            bundle: mut raw,
            bcbs: bcb_ops,
            bibs: mut bib_ops,
        } = parse_headers_only(full);
        let mut decrypted = HashMap::new();
        let no_updates = HashMap::new();
        let facts = checks::verify(
            &consumed,
            keys,
            &mut raw.blocks,
            &bcb_ops,
            &mut bib_ops,
            &mut decrypted,
            &no_updates,
        )
        .unwrap();
        (consumed, raw, facts)
    }

    // The streamed twin of `payload_bib_deferred_then_verified`: the
    // deferred op-set becomes an incremental verifier constructed from the
    // headers-only buffer, fed the payload's block-type-specific data in
    // awkward chunk sizes, and settles Ok — no resident payload anywhere.
    #[test]
    fn payload_bib_verifies_incrementally() {
        let full = signed_large_payload();
        let keys = keys();
        let (consumed, raw, facts) = deferred_setup(&full, &keys);
        let bib_block = *facts.deferred_bibs.keys().next().expect("a deferred BIB");

        let mut verifiers =
            checks::begin_payload_verification(&consumed, &keys, &raw.blocks, &facts.deferred_bibs)
                .expect("verifier construction needs only header material");
        assert_eq!(verifiers.len(), 1);
        let (got_bib, mut verifier) = verifiers.pop().unwrap();
        assert_eq!(
            got_bib, bib_block,
            "failure attribution rides the BIB number"
        );

        // Feed exactly the payload's block-type-specific data, as the drain
        // would: in deliberately awkward chunk sizes. `payload_range` is the
        // bundle-absolute window (`data` alone is extent-relative).
        let data_range = raw.blocks.get(&1).unwrap().payload_range();
        let btsd = &full[data_range.start as usize..data_range.end as usize];
        for chunk in btsd.chunks(7) {
            verifier.update(chunk);
        }
        verifier.finish().expect("streamed payload BIB verifies");
    }

    // A tampered streamed byte fails at finish() with IntegrityCheckFailed —
    // the streamed twin of `payload_bib_tamper_fails`.
    #[test]
    fn payload_bib_incremental_tamper_fails() {
        let full = signed_large_payload();
        let keys = keys();
        let (consumed, raw, facts) = deferred_setup(&full, &keys);

        let mut verifiers =
            checks::begin_payload_verification(&consumed, &keys, &raw.blocks, &facts.deferred_bibs)
                .unwrap();
        let (_, mut verifier) = verifiers.pop().unwrap();

        let data_range = raw.blocks.get(&1).unwrap().payload_range();
        let mut btsd = full[data_range.start as usize..data_range.end as usize].to_vec();
        let mid = btsd.len() / 2;
        btsd[mid] ^= 0xFF;
        for chunk in btsd.chunks(1024) {
            verifier.update(chunk);
        }
        let err = verifier
            .finish()
            .expect_err("tampered streamed payload must fail");
        assert!(
            matches!(err, bpsec::Error::IntegrityCheckFailed),
            "expected IntegrityCheckFailed, got {err:?}"
        );
    }

    // No usable key is a soft policy skip: construction yields no verifiers
    // rather than an error.
    #[test]
    fn payload_bib_incremental_nokey_skips() {
        let full = signed_large_payload();
        let keys = keys();
        let (consumed, raw, facts) = deferred_setup(&full, &keys);

        let empty_keys = bpsec::key::KeySet::new(vec![]);
        let verifiers = checks::begin_payload_verification(
            &consumed,
            &empty_keys,
            &raw.blocks,
            &facts.deferred_bibs,
        )
        .expect("NoKey is a soft skip, not an error");
        assert!(verifiers.is_empty());
    }

    // The verifier must be able to cross await points and task boundaries:
    // it deliberately owns its key material (the recorded exception to the
    // header pass's key-handling rule).
    #[test]
    fn bib_verifier_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<bpsec::bib::Verifier>();
    }

    // The `DeferredBibs` accessors must report the deferred set truthfully on
    // the non-empty side: the assert-empty discharge sites at the all-resident
    // callers enforce nothing if `is_empty` can lie. On the headers-only
    // buffer the standalone verifier defers exactly the block-1 BIB — the set
    // is non-empty and `iter` names that block.
    #[test]
    fn verify_all_bibs_defers_nonempty_on_headers_only_buffer() {
        let full = signed_large_payload();
        let keys = keys();

        let Parsed {
            data: consumed,
            bundle: raw,
            bibs: bib_ops,
            ..
        } = parse_headers_only(&full);
        let bib_block = *bib_ops.keys().next().expect("a BIB op-set");

        let no_decrypted = HashMap::new();
        let no_updates = HashMap::new();
        let deferred = checks::verify_all_bibs(
            &consumed,
            &keys,
            &raw.blocks,
            &bib_ops,
            &no_decrypted,
            &no_updates,
        )
        .expect("the non-resident payload target defers, it does not fail");
        assert!(
            !deferred.is_empty(),
            "the block-1 BIB must be reported as deferred"
        );
        assert_eq!(deferred.iter().collect::<Vec<_>>(), vec![bib_block]);
    }

    // With the whole bundle resident, `verify` checks the payload BIB inline and
    // defers nothing — the non-streaming path is unchanged.
    #[test]
    fn all_resident_verifies_inline_without_deferring() {
        let full = signed_large_payload();
        let keys = keys();

        let (data, mut raw, bcb_ops, mut bib_ops) =
            raw_parse_tuple(Bytes::copy_from_slice(&full)).unwrap();
        let mut decrypted = HashMap::new();
        let no_updates = HashMap::new();
        let facts = checks::verify(
            &data,
            &keys,
            &mut raw.blocks,
            &bcb_ops,
            &mut bib_ops,
            &mut decrypted,
            &no_updates,
        )
        .unwrap();
        assert!(
            facts.deferred_bibs.is_empty(),
            "nothing is deferred when the payload is resident"
        );
    }
}
