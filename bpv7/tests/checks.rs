//! Integration tests for the BPSec validation/rewrite pipeline — composing
//! the public `hardy_bpv7::{parse, checks, rewrite}` primitives the way a
//! consumer would (mirrors the reference pipeline in `bpa::bp7_parse`).

use bytes::Bytes;
use hardy_bpv7::parse::Parsed;
use hardy_bpv7::{
    Bundle, Error, block, bpsec, builder, checks, crc, creation_timestamp, editor, eid, hop_info,
    parse, rewrite,
};
use hex_literal::hex;
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
    Ok((data, bundle, bcbs, bibs))
}

// Requirement: LLR 1.1.15
#[test]
fn invalid_flags() {
    // From Stephan Havermans testing. The bundle's primary block has a
    // null source EID and one of its extension blocks has
    // `report_on_failure` set — forbidden by RFC 9171 §4.2.3.
    //
    // `parse` catches this during structural parsing and bails before a
    // `Bundle` is constructed, so the error propagates as a bare
    // `Err(Error::InvalidFlags)`.
    const BUNDLE: &[u8] = &hex!(
        "9f89071844018202820301820100820100821b000000b5998c982b011a000493e042c9f6850602182700458202820200850704010042183485010101004454455354ff"
    );
    assert!(matches!(
        raw_parse_tuple(Bytes::from_static(BUNDLE)),
        Err(Error::InvalidFlags)
    ));
}

// NOTE: LLR 1.1.33 (Bundle Age required when Creation Time is zero) is now enforced
// by the BPA rfc9171-filter, not the parser. Parser accepts such bundles to allow
// compatibility with RFC9173 test vectors.

fn empty_keys() -> bpsec::key::KeySet {
    bpsec::key::KeySet::new(vec![])
}

// Full-mode pipeline: composes the per-section helpers exactly as
// `bpa::bp7_parse` does, so the cascade tests exercise the real
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
    // non-payload → remove the target and its security block.
    for &target in &facts.failed {
        if target == 1 {
            return Err(bpsec::Error::DecryptionFailed.into());
        }
        to_remove.insert(target);
        if let Some(bcb) = raw.blocks.get(&target).and_then(|b| b.bcb) {
            to_remove.insert(bcb);
        }
    }
    for (_, block_type) in &facts.nokey_ext {
        match block_type {
            block::Type::HopCount => return Err(bpsec::Error::NoKey.into()),
            block::Type::BundleAge if !raw.primary.id.timestamp.is_clocked() => {
                return Err(bpsec::Error::NoKey.into());
            }
            _ => {}
        }
    }

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

// Requirement: LLR 1.1.34
#[test]
fn hop_count_extraction() {
    let hop = hop_info::HopInfo {
        limit: 30,
        count: 0,
    };
    let (_, data) = builder::Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
        .with_hop_count(&hop)
        .with_payload("Hello".as_bytes().into())
        .build(creation_timestamp::CreationTimestamp::now())
        .unwrap();

    let (data, raw_bundle, _, _) = raw_parse_tuple(Bytes::copy_from_slice(&data)).unwrap();
    // Decode the HopCount block body directly via its bpv7 CBOR type.
    let hc_block = raw_bundle
        .blocks
        .values()
        .find(|b| matches!(b.block_type, block::Type::HopCount))
        .expect("HopCount block present");
    let body = hc_block.payload(&data).expect("HopCount body in bundle");
    let hop_count = hardy_cbor::decode::parse::<hop_info::HopInfo>(body).unwrap();
    assert_eq!(hop_count.limit, 30);
    assert_eq!(hop_count.count, 0);
}

// Requirement: LLR 1.1.19
#[test]
fn extension_block_parsing() {
    // Build a bundle with hop count — verifies HopCount extension is parsed
    let hop = hop_info::HopInfo {
        limit: 10,
        count: 3,
    };
    let (_, data) = builder::Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
        .with_hop_count(&hop)
        .with_payload("Test".as_bytes().into())
        .build(creation_timestamp::CreationTimestamp::now())
        .unwrap();

    let (_data, raw_bundle, _, _) = raw_parse_tuple(Bytes::copy_from_slice(&data)).unwrap();

    // HopCount block present (interpretation into a typed field is a
    // hardy-bpa concern now — here we just confirm the block parsed).
    assert!(
        raw_bundle
            .blocks
            .values()
            .any(|b| matches!(b.block_type, block::Type::HopCount))
    );

    // Payload block exists
    assert!(raw_bundle.blocks.contains_key(&1));
}

// Requirement: LLR 1.1.12
#[test]
fn truncated_bundle() {
    let data = build_minimal_bundle();

    // Truncation detection lives at the CBOR layer — test it directly via
    // `parse` rather than the canonicalize/Parsed orchestrators (whose
    // truncation behaviour is just whatever the parser returns).
    for len in [0, 1, 2, 5, data.len() / 2, data.len() - 1] {
        assert!(
            raw_parse_tuple(Bytes::copy_from_slice(&data[..len])).is_err(),
            "parse: truncated at {len} bytes should fail"
        );
    }
}

// Requirement: Trailing Data
#[test]
fn trailing_data() {
    let data = build_minimal_bundle();
    let mut with_trailing = data.to_vec();
    with_trailing.push(0xFF);

    // Trailing data is detected by `parse::finish` (payload extent doesn't
    // reach the end of `data`). The check lives at the structural-parse
    // layer; every higher orchestrator just propagates the resulting
    // `Err(AdditionalData)`.
    assert!(
        matches!(
            raw_parse_tuple(Bytes::copy_from_slice(&with_trailing)),
            Err(Error::AdditionalData)
        ),
        "parse with trailing data should return Err(AdditionalData)"
    );
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
        !classification.report_unsupported,
        "Builder has no unsupported blocks"
    );
}

// Requirement: LLR 1.1.14
//
// RFC 9171 §4.1 (normative) requires the bundle to be a CBOR
// indefinite-length array — the only legal outer-array head byte is 0x9F.
// Appendix B's CDDL `bpv7_start = bundle / #6.55799(bundle)` permits a
// self-describing tag wrapper but is explicitly informational and
// subordinate to the textual spec; the parser therefore rejects the tag
// form outright rather than treating it as a non-canonical encoding.
#[test]
fn non_canonical_rewriting_rejects_outer_tag() {
    let data = build_minimal_bundle();
    assert_eq!(
        data[0], 0x9F,
        "Bundle should start with indefinite array marker"
    );

    let mut tagged = Vec::with_capacity(data.len() + 3);
    tagged.extend_from_slice(&[0xD9, 0xD9, 0xF7]); // Tag 55799
    tagged.extend_from_slice(&data);

    // The tag-wrapped form is rejected at the parse layer
    // (`slow_bundle_array_error` — the first byte isn't 0x9F).
    assert!(matches!(
        raw_parse_tuple(Bytes::copy_from_slice(&tagged)),
        Err(Error::InvalidCBOR(
            hardy_cbor::decode::Error::IncorrectType(..)
        ))
    ));
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

// Requirement: LLR 1.1.22
#[test]
fn crc16_bundle() {
    let (_, data) = builder::Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
        .with_crc_type(crc::CrcType::CRC16_X25)
        .with_payload("CRC16 test".as_bytes().into())
        .build(creation_timestamp::CreationTimestamp::now())
        .unwrap();

    // Parse and verify CRC type via parse (primary block field).
    let (_data, raw_bundle, _, _) = raw_parse_tuple(Bytes::copy_from_slice(&data)).unwrap();
    assert!(
        matches!(raw_bundle.primary.crc_type, crc::CrcType::CRC16_X25),
        "CRC type should be CRC-16"
    );
}

// Requirement: LLR 1.1.1
#[test]
fn ccsds_compliance() {
    // CCSDS 734.20-O-1 requires BPv7 per RFC 9171:
    // - Indefinite-length outer array
    // - Version 7 in primary block
    // - Valid CRC on primary block
    // - Payload block present
    let data = build_minimal_bundle();

    // Indefinite-length array marker
    assert_eq!(data[0], 0x9F, "Bundle must use indefinite-length array");

    // Break code at end
    assert_eq!(
        data[data.len() - 1],
        0xFF,
        "Bundle must end with break code"
    );

    // Parse and verify structural compliance via primitives.
    let (_data, raw_bundle, _, _) = raw_parse_tuple(Bytes::copy_from_slice(&data)).unwrap();
    assert!(
        raw_bundle.blocks.contains_key(&1),
        "Payload block (block 1) must be present"
    );
    assert!(
        !matches!(raw_bundle.primary.crc_type, crc::CrcType::None),
        "Primary block must have a CRC"
    );
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
