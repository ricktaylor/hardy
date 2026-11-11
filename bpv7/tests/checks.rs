//! Integration tests for the BPSec validation/rewrite pipeline — composing
//! the public `hardy_bpv7::{parse, checks, rewrite}` primitives the way a
//! consumer would (mirrors the reference pipeline in `bpa::bundle::parse`).

use bytes::Bytes;
use hardy_bpv7::parse::Parsed;
use hardy_bpv7::{
    Bundle, Error, block,
    bpsec::{
        self, Decrypted, Role, Roles, SecurityService, SecurityServices,
        checks::{self, Checked},
    },
    builder,
    bundle::FragmentInfo,
    crc, creation_timestamp,
    editor::{self, Chunk, Editor},
    eid, parse,
    reader::PlainReader,
    rewrite,
};
use std::collections::{HashMap, HashSet};

mod common;
use self::common::{insert_after_primary, make_block, make_unknown_context_asb, rand_k};

// The operations `facts` found `checked`, as (security block, target), in
// block-number order.
fn found(facts: &checks::Facts, checked: Checked) -> Vec<(u64, u64)> {
    let mut found: Vec<(u64, u64)> = facts
        .outcomes
        .iter()
        .filter(|o| o.checked == checked)
        .map(|o| (o.security_block, o.target))
        .collect();
    found.sort_unstable();
    found
}

// The targets of the operations `facts` found `checked`, in order.
fn found_targets(facts: &checks::Facts, checked: Checked) -> Vec<u64> {
    let mut targets: Vec<u64> = facts.targets(checked).collect();
    targets.sort_unstable();
    targets
}

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
        ..
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

// The pipeline's failures span the parse, the checks and the rewrite, whose
// error types differ; the tests only display them.
type PipelineResult<T> = Result<T, Box<dyn std::error::Error>>;

// Full-mode pipeline: composes the per-section helpers exactly as
// `bpa::bundle::parse` does, so the cascade tests exercise the real
// composition. Returns the (possibly rewritten) bundle plus the chunk
// plan when rewrites were applied.
fn parse_full_for_test(
    data: &[u8],
    keys: &bpsec::key::KeySet,
) -> PipelineResult<(Bundle, Option<Vec<editor::Chunk>>)> {
    parse_full_accepting(data, keys, &HashSet::new())
}

// As `parse_full_for_test`, at a node that accepts the blocks in
// `acceptors`, so an encrypted BIB that may cover one is decrypted.
fn parse_full_accepting(
    data: &[u8],
    keys: &bpsec::key::KeySet,
    acceptors: &HashSet<u64>,
) -> PipelineResult<(Bundle, Option<Vec<editor::Chunk>>)> {
    let mut parsed = parse::parse(Bytes::copy_from_slice(data))?;

    if let Some(error) = parsed.classification.delete_bundle.take() {
        return Err(error.into());
    }
    let mut to_remove: HashSet<u64> = HashSet::new();
    to_remove.extend(parsed.classification.unrecognised_deletable.iter().copied());
    to_remove.extend(parsed.classification.bib_deletable.iter().copied());

    let to_update: HashMap<u64, Vec<u8>> = HashMap::new();
    let facts = common::verify_as_receiver(&mut parsed, keys, acceptors)?;
    // RFC 9172 §5.1.1: a corrupt target is removed. The editor cascade
    // strips it from its covering BCB and drops the BCB once it empties;
    // naming the BCB here would strand a surviving co-target's ciphertext.
    let failed = |service| facts.targets_of(service, Checked::Failed);
    to_remove.extend(failed(SecurityService::Confidentiality));
    // A failed integrity check rejects the bundle.
    if failed(SecurityService::Integrity).next().is_some() {
        return Err(bpsec::Error::IntegrityCheckFailed.into());
    }
    // `check` itself fails a bundle without a creation time whose Bundle
    // Age it cannot read; any other NoKey policy is the caller's, and no
    // fixture in this file produces a `NoKey` outcome for a BCB operation.

    // §D (extension-field extraction / canonical re-emit) is a BPA concern
    // and now lives in `hardy-bpa`; the cascade-test bundles carry no
    // PreviousNode/HopCount, so there are no canonical re-emits to queue here.

    let Parsed {
        data,
        bundle: mut raw,
        ..
    } = parsed;
    let chunks = if to_update.is_empty() && to_remove.is_empty() {
        None
    } else {
        rewrite::apply_rewrites(
            Editor::new(&raw, &data).with_key_source(keys),
            to_update,
            to_remove,
        )?
        .map(|(new_raw, chunks)| {
            raw = new_raw;
            chunks
        })
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
    let parsed = parse::parse(Bytes::copy_from_slice(&data)).unwrap();
    let raw_bundle = &parsed.bundle;
    assert_eq!(raw_bundle.primary.id.source, original.primary.id.source);
    assert_eq!(raw_bundle.primary.destination, original.primary.destination);
    assert_eq!(raw_bundle.primary.report_to, original.primary.report_to);
    assert_eq!(raw_bundle.primary.lifetime, original.primary.lifetime);
    assert!(parsed.bcbs.is_empty(), "Builder output has no BCBs");
    assert!(parsed.bibs.is_empty(), "Builder output has no BIBs");
    // Full mode would additionally classify unrecognised/unsupported
    // blocks. Builder output has none, so no deletables either.
    let classification = &parsed.classification;
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

// Two unrecognised blocks demanding the bundle's deletion: the verdict names
// the lowest-numbered one, whatever order the parse's maps visit them in.
#[test]
fn a_deletion_verdict_names_the_lowest_block() {
    let data = build_minimal_bundle();
    // delete_bundle_on_failure (0x04) on blocks 3 and 2, block 3 first on
    // the wire.
    let later = make_block(999, 3, 0x04, &[0xDE, 0xAD]);
    let earlier = make_block(998, 2, 0x04, &[0xBE, 0xEF]);
    let modified = insert_after_primary(&data, &[&later, &earlier]);
    let parsed = parse::parse(Bytes::copy_from_slice(&modified)).expect("the blocks parse");
    assert!(
        matches!(
            parsed.classification.delete_bundle,
            Some(Error::Unsupported(2))
        ),
        "{:?}",
        parsed.classification.delete_bundle
    );
}

// Requirement: LLR 1.1.30
#[test]
fn unknown_block_discard() {
    let data = build_minimal_bundle();

    // Insert an unknown extension block (type 999) with delete_block_on_failure flag
    // between the primary block and the payload block.
    let unknown_block = make_block(999, 2, 0x10, &[0xDE, 0xAD, 0xBE, 0xEF]);
    let modified = insert_after_primary(&data, &[&unknown_block]);

    // Preserve-mode semantics demonstrated via primitives: parse keeps every
    // block, `classify` identifies block 2 as deletable, and a Preserve-mode
    // caller ignores the deletable list (block 2 stays).
    let parsed =
        parse::parse(Bytes::copy_from_slice(&modified)).expect("parse accepts the unknown block");
    assert!(
        parsed.bundle.blocks.contains_key(&2),
        "parse should preserve unknown block 2"
    );
    let classification = &parsed.classification;
    assert!(
        classification.delete_bundle.is_none(),
        "unknown block has no delete_bundle_on_failure flag"
    );
    assert!(
        classification.unrecognised_deletable.contains(&2),
        "block 2 is marked deletable (delete_block_on_failure flag set) — \
         Preserve-mode callers ignore this list"
    );

    // Full-mode end-to-end smoke check via composed primitives — verifies
    // that the deletable list produced by `classify` actually flows through
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
    insert_after_primary(data, &[&bcb_block])
}

// Requirement: RFC 9172 §7.1 — the §A facts distinguish an unsupported
// security operation from an unrecognised plain block, so the caller can
// select between the RFC 9172 and RFC 9171 report reasons.
#[test]
fn classify_distinguishes_security_kind() {
    // Unrecognised-context BCB: must_replicate (payload target) +
    // report_on_failure (0x03) → only the security fact fires.
    let modified = splice_unrecognised_bcb(&build_minimal_bundle(), 0x03);
    let classification = parse::parse(Bytes::copy_from_slice(&modified))
        .expect("parse accepts an unrecognised-context BCB")
        .classification;
    assert!(classification.report_unsupported_security);
    assert!(!classification.report_unsupported_block);

    // Control: an unknown (non-security) block with report_on_failure (0x02)
    // → only the block fact fires.
    let unknown_block = make_block(999, 2, 0x02, &[0xDE, 0xAD]);
    let modified = insert_after_primary(&build_minimal_bundle(), &[&unknown_block]);
    let classification = parse::parse(Bytes::copy_from_slice(&modified))
        .unwrap()
        .classification;
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
    let parsed = parse::parse(Bytes::copy_from_slice(&modified)).unwrap();
    assert!(matches!(
        parsed.classification.delete_bundle,
        Some(Error::InvalidBPSec(bpsec::Error::UnrecognisedContext(99)))
    ));
}

// Requirement: RFC 9172 §7.1 — an unsupported security operation flagged
// report_on_failure and delete_bundle_on_failure raises its
// `UnknownSecurityOperation` report fact alongside the deletion verdict, for
// a BCB (A2) and for a plaintext BIB (A3) alike.
#[test]
fn classify_reports_the_security_facts_with_the_delete_verdict() {
    // BCB: must_replicate (payload target) + report + delete bundle (0x07).
    let bcb = splice_unrecognised_bcb(&build_minimal_bundle(), 0x07);
    // BIB: report + delete bundle (0x06).
    let bib = insert_after_primary(
        &build_minimal_bundle(),
        &[&make_block(11, 2, 0x06, &make_unknown_context_asb(1))],
    );
    for (kind, modified) in [("BCB", bcb), ("BIB", bib)] {
        let classification = parse::parse(Bytes::copy_from_slice(&modified))
            .unwrap()
            .classification;
        assert!(
            classification.report_unsupported_security,
            "{kind}: the report fact survives the deletion verdict"
        );
        assert!(
            matches!(
                classification.delete_bundle,
                Some(Error::InvalidBPSec(bpsec::Error::UnrecognisedContext(99)))
            ),
            "{kind}: got {:?}",
            classification.delete_bundle
        );
    }
}

// Requirement: RFC 9171 §5.6 Step 4 reports, then deletes — the facts and
// the delete-bundle verdict come back together, so a block's
// `report_on_failure` demand survives the deletion its
// `delete_bundle_on_failure` orders.
#[test]
fn classify_reports_the_facts_with_the_delete_verdict() {
    // An unknown (non-security) block flagged report_on_failure and
    // delete_bundle_on_failure (0x06), beside an unrecognised-context BIB and
    // another unknown block, both flagged delete_block_on_failure (0x10): the
    // deletion verdict leaves the classification of every other block
    // complete. The BIB's arm (A3) runs after the verdict's (A1).
    let unknown_block = make_block(999, 2, 0x06, &[0xDE, 0xAD]);
    let deletable_bib = make_block(11, 3, 0x10, &make_unknown_context_asb(1));
    let deletable_block = make_block(998, 4, 0x10, &[0xBE, 0xEF]);
    let modified = insert_after_primary(
        &build_minimal_bundle(),
        &[&unknown_block, &deletable_bib, &deletable_block],
    );
    let classification = parse::parse(Bytes::copy_from_slice(&modified))
        .unwrap()
        .classification;
    assert!(classification.report_unsupported_block);
    assert_eq!(classification.bib_deletable.as_slice(), &[3]);
    assert_eq!(classification.unrecognised_deletable.as_slice(), &[4]);
    assert!(matches!(
        classification.delete_bundle,
        Some(Error::Unsupported(2))
    ));
}

// Requirement: RFC 9171 §5.6 Step 4 — a plaintext BIB with an unrecognised
// security context is the classification's, kept or removed as its flags
// decide. Neither `check` nor the payload's streaming verification checks
// it (the latter refuses to begin), flagged `delete_block_on_failure` or not, so the caller need not take
// it out of `bib_ops` first.
#[test]
fn an_unsupported_bib_is_left_to_the_classification() {
    for (flags, deletable) in [(0x00, false), (0x10, true)] {
        let modified = insert_after_primary(
            &build_minimal_bundle(),
            &[&make_block(11, 2, flags, &make_unknown_context_asb(1))],
        );
        let mut parsed = parse::parse(Bytes::from(modified)).unwrap();

        let classification = &parsed.classification;
        assert_eq!(
            classification.bib_deletable.contains(&2),
            deletable,
            "flags {flags:#x}"
        );

        let ops = &parsed.bibs[&2];
        assert!(
            matches!(
                ops.operations()[&1].begin_verify(
                    &bpsec::key::KeySet::EMPTY,
                    &bpsec::bib::OperationArgs {
                        source: ops.source(),
                        target: 1,
                        security_block: 2,
                        blocks: &PlainReader {
                            blocks: &parsed.bundle.blocks,
                            source_data: &parsed.data,
                        },
                    },
                    0,
                ),
                Err(bpsec::Error::UnrecognisedContext(99))
            ),
            "flags {flags:#x}: the payload stream checks nothing"
        );

        let facts = verify_with(&mut parsed, vec![])
            .unwrap_or_else(|e| panic!("flags {flags:#x}: verify checks nothing: {e}"));
        assert!(
            !facts.any(Checked::Failed) && !facts.any(Checked::NoKey),
            "flags {flags:#x}"
        );
    }
}

// End-to-end tests for the BCB-covered BIB re-encryption cascade through
// `parse_full_for_test` → `rewrite::apply_rewrites` →
// `bpsec::edit::BPSecEditor::remove_blocks` (which internally calls the
// private `reencrypt_covered_bib`). Requires rfc9173 (for BCB-AES-GCM +
// BIB-HMAC-SHA2) and serde (for JWK deserialisation).
#[cfg(all(feature = "rfc9173", feature = "serde"))]
mod cascade_reencryption_tests {
    use zeroize::Zeroizing;

    use super::*;

    // Fresh per call: a test binds the key once and passes it to every
    // path that must agree on it.
    fn sign_key() -> bpsec::key::Key {
        serde_json::from_value(serde_json::json!({
            "kid": "ipn:2.1",
            "kty": "oct",
            "alg": "HS256",
            "key_ops": ["sign", "verify"],
            "k": rand_k(18)
        }))
        .unwrap()
    }

    // Wrong-key tests bind the `k` value explicitly so distinctness from
    // the wrong key can be asserted structurally.
    fn enc_key_with_k(k: &str) -> bpsec::key::Key {
        serde_json::from_value(serde_json::json!({
            "kid": "ipn:2.1",
            "kty": "oct",
            "alg": "A128KW",
            "enc": "A128GCM",
            "key_ops": ["encrypt", "decrypt", "wrapKey", "unwrapKey"],
            "k": k
        }))
        .unwrap()
    }

    fn enc_key() -> bpsec::key::Key {
        enc_key_with_k(&rand_k(16))
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
        // delete_block_on_failure (0x10)
        let unknown = make_block(999, 2, 0x10, &[0xDE, 0xAD]);
        insert_after_primary(&base, &[&unknown])
    }

    // A bundle with a payload plus an unknown extension block (type 999,
    // block #2) that carries a CRC-32.
    fn build_with_crc_block() -> Box<[u8]> {
        let (_, data) =
            builder::Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
                .add_extension_block(block::Type::Unrecognised(999))
                .unwrap()
                .with_crc_type(crc::CrcType::CRC32_CASTAGNOLI)
                .build([0xDE, 0xAD].as_slice().into())
                .with_payload(b"payload data".as_slice().into())
                .build(creation_timestamp::CreationTimestamp::now())
                .unwrap();
        data
    }

    // As `build_with_unknown_block`, plus a second unknown extension block
    // (type 998, block #3).
    fn build_with_unknown_blocks() -> Vec<u8> {
        let (_, base) =
            builder::Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
                .with_payload(b"payload data".as_slice().into())
                .build(creation_timestamp::CreationTimestamp::now())
                .unwrap();
        insert_after_primary(
            &base,
            &[
                &make_block(999, 2, 0x10, &[0xDE, 0xAD]),
                &make_block(998, 3, 0x10, &[0xBE, 0xEF]),
            ],
        )
    }

    // Sign the named targets under a single BIB (HMAC-SHA2, source ipn:2.1)
    // and return the rebuilt bytes. The scope leaves the security header
    // out, as BIB-HMAC-SHA2 requires for targets to share a BIB.
    fn sign(bundle_bytes: &[u8], targets: &[u64], key: &bpsec::key::Key) -> Box<[u8]> {
        let (bundle_bytes, raw, _, _) =
            raw_parse_tuple(Bytes::copy_from_slice(bundle_bytes)).expect("parse");
        let mut signer = bpsec::signer::Signer::new(&raw, &bundle_bytes);
        for &t in targets {
            signer = signer
                .sign_block(
                    t,
                    bpsec::signer::Context::HMAC_SHA2(bpsec::rfc9173::ScopeFlags {
                        include_security_header: false,
                        ..bpsec::rfc9173::ScopeFlags::default()
                    }),
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
        encrypt_as(bundle_bytes, target, "ipn:2.1", enc_k)
    }

    // As `encrypt`, under the security source `source`.
    fn encrypt_as(
        bundle_bytes: &[u8],
        target: u64,
        source: &str,
        enc_k: &bpsec::key::Key,
    ) -> Box<[u8]> {
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
                source.parse().unwrap(),
                enc_k,
            )
            .map_err(|(_, e)| e)
            .unwrap();
        encryptor.rebuild().unwrap()
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

    // Removing a block from an encrypted BIB that also covers a block that
    // stays would shrink the BIB, which would then have to be encrypted
    // again, as only its BCB's security source may: the cascade refuses,
    // whatever keys it holds, naming the block and the BIB. Naming the BIB
    // too removes it whole, and the payload keeps its ciphertext.
    #[test]
    fn cascade_refuses_to_shrink_an_encrypted_bib() {
        let sign_k = sign_key();
        let enc_k = enc_key();
        let keys = bpsec::key::KeySet::new(vec![sign_k.clone(), enc_k.clone()]);

        // Build → sign(payload + unknown) → encrypt(payload). The encryptor
        // encrypts the BIB and the unknown block too, each AES-GCM operation
        // in its own BCB.
        let with_unknown = build_with_unknown_block();
        let signed = sign(&with_unknown, &[1, 2], &sign_k);
        let encrypted = encrypt(&signed, 1, &enc_k);
        let parsed = parse::parse(Bytes::copy_from_slice(&encrypted)).unwrap();
        let bib = find_bib(&parsed.bundle).expect("BIB present");
        assert!(
            parsed.bundle.blocks[&bib].bcb.is_some(),
            "the BIB is encrypted"
        );

        let refused = Editor::new(&parsed.bundle, &parsed.data)
            .with_key_source(&keys)
            .remove_blocks(&HashSet::from([2]))
            .map(|_| ())
            .map_err(|(_, e)| e);
        assert!(
            matches!(
                refused,
                Err(editor::Error::ShrinksEncryptedBib { block: 2, bib: b }) if b == bib
            ),
            "{refused:?}"
        );

        let (editor, removed) = Editor::new(&parsed.bundle, &parsed.data)
            .with_key_source(&keys)
            .remove_blocks(&HashSet::from([2, bib]))
            .map_err(|(_, e)| e)
            .expect("naming the BIB removes it whole");
        assert_eq!(removed, HashSet::from([2, bib]));
        let rebuilt = parse::parse(Bytes::from(editor::Chunk::flatten(
            editor.rebuild().unwrap(),
            &parsed.data,
        )))
        .unwrap();
        assert!(find_bib(&rebuilt.bundle).is_none(), "the BIB is gone");
        assert_eq!(
            rebuilt.bundle.blocks[&1].payload(&rebuilt.data),
            parsed.bundle.blocks[&1].payload(&parsed.data),
            "the payload keeps its ciphertext"
        );
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
        // Bind both `k` values so the wrong-key premise is structural.
        let enc_k_val = rand_k(16);
        let wrong_k_val = rand_k(16);
        assert_ne!(
            enc_k_val, wrong_k_val,
            "the wrong key must differ from the encryption key"
        );
        let enc_k = enc_key_with_k(&enc_k_val);

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
        // DecryptionFailed (not NoKey) when the payload's acceptor decrypts
        // the BIB over it.
        let wrong_enc_k = enc_key_with_k(&wrong_k_val);
        let wrong_keys = bpsec::key::KeySet::new(vec![sign_k, wrong_enc_k]);

        // The pipeline applies §5.1.1 failure-drop: the corrupt BIB and the
        // BCB that was protecting it are removed. The payload block survives
        // (it remains BCB-encrypted under its own separate BCB, which is left
        // intact — the payload itself is not corrupt).
        let (bundle, _chunks) = parse_full_accepting(&encrypted, &wrong_keys, &HashSet::from([1]))
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
        // The keys can decrypt, so the byte flip is what breaks
        // verification: the pristine vector decrypts and verifies cleanly.
        let keys = rfc9173_a4_keys();
        let mut data = rfc9173_a4_vector();
        // As the payload's acceptor, which decrypts the BIB over it.
        let acceptors = HashSet::from([1]);
        let (pristine, _) = parse_full_accepting(&data, &keys, &acceptors)
            .expect("the pristine A.4 vector must parse cleanly with the pinned keys");
        assert!(pristine.blocks.contains_key(&3), "pristine BIB survives");

        corrupt_a4_bib(&mut data);
        let (bundle, chunks) = parse_full_accepting(&data, &keys, &acceptors)
            .expect("§5.1.1 failure-drop: bundle survives a corrupt target of a shared BCB");
        assert!(chunks.is_some(), "the bundle must be rewritten");
        assert!(!bundle.blocks.contains_key(&3), "corrupt BIB dropped");
        assert!(
            bundle.blocks.contains_key(&2),
            "shared BCB survives, still covering the payload"
        );
        assert!(bundle.blocks.contains_key(&1), "payload survives");
    }

    // `apply_rewrites` returns the removal cascade's failure rather than
    // panicking. A node that accepts block 2, whose keyed verification
    // resolved its coverage to its encrypted BIB, asked to remove block 2
    // without the key that decrypts the BIB, cannot strip the block from it.
    #[test]
    fn apply_rewrites_returns_a_cascade_failure() {
        let enc_k = enc_key();
        let encrypted = encrypt(
            &sign(&build_with_unknown_block(), &[2], &sign_key()),
            2,
            &enc_k,
        );
        let mut parsed = parse::parse(Bytes::copy_from_slice(&encrypted)).expect("parse");
        let facts = common::verify_as_receiver(
            &mut parsed,
            &bpsec::key::KeySet::new(vec![enc_k]),
            &HashSet::from([2]),
        )
        .expect("the keyed verification decrypts the BIB");
        assert!(!facts.any(Checked::Failed));
        let raw = &parsed.bundle;
        let bib = find_bib(raw).expect("BIB present");
        assert_eq!(raw.blocks[&2].bib, block::BibCoverage::Some(bib));

        assert!(matches!(
            rewrite::apply_rewrites(
                Editor::new(raw, &parsed.data),
                HashMap::new(),
                HashSet::from([2])
            ),
            Err(editor::Error::Builder(builder::Error::InternalError(
                Error::InvalidField {
                    field: "BIB Abstract Syntax Block",
                    ..
                }
            )))
        ));
    }

    // The cascade checks the BIB it decrypts, as remove_encryption, an
    // editor resolving a block's coverage, and the keyed verification of a
    // node that accepts block 2 do. The Signer and
    // Encryptor cannot make an encrypted BIB that lists a block the bundle
    // lacks, so the bytes are cut by hand: block 3 and the BCB encrypting
    // it go from a valid bundle whose encrypted BIB targets blocks 2 and 3.
    // Neither the parse nor a keyless reader can see that the BIB still
    // lists block 3.
    #[test]
    fn decrypted_bib_with_a_missing_target_fails() {
        let enc_k = enc_key();
        let keys = bpsec::key::KeySet::new(vec![enc_k.clone()]);
        let encrypted = encrypt(
            &sign(&build_with_unknown_blocks(), &[2, 3], &sign_key()),
            2,
            &enc_k,
        );

        let (_, raw, _, _) = raw_parse_tuple(Bytes::copy_from_slice(&encrypted)).expect("parse");
        let bcb = raw.blocks[&3]
            .bcb
            .expect("the encryptor encrypts the BIB's other targets");
        let mut extents = [3, bcb].map(|n| raw.blocks[&n].extent.clone());
        extents.sort_by_key(|extent| extent.start);
        let mut cut = encrypted.to_vec();
        for extent in extents.into_iter().rev() {
            cut.drain(usize::try_from(extent.start).unwrap()..usize::try_from(extent.end).unwrap());
        }

        let mut parsed =
            parse::parse(Bytes::from(cut)).expect("the parse cannot read the BIB's targets");
        let raw = &parsed.bundle;
        let data = &parsed.data;
        assert!(!raw.blocks.contains_key(&3));
        let bib = find_bib(raw).expect("BIB present");
        assert!(raw.blocks[&bib].bcb.is_some());

        assert!(matches!(
            rewrite::apply_rewrites(
                Editor::new(raw, data).with_key_source(&keys),
                HashMap::new(),
                HashSet::from([2])
            ),
            Err(editor::Error::Bpsec(bpsec::Error::MissingSecurityTarget))
        ));
        assert!(matches!(
            Editor::new(raw, data)
                .with_key_source(&keys)
                .remove_encryption(2),
            Err((_, editor::Error::Bpsec(bpsec::Error::MissingSecurityTarget)))
        ));
        assert!(matches!(
            Editor::new(raw, data)
                .with_key_source(&keys)
                .update_block(2),
            Err((_, editor::Error::Bpsec(bpsec::Error::MissingSecurityTarget)))
        ));
        assert!(matches!(
            common::verify_as_receiver(&mut parsed, &keys, &HashSet::from([2])),
            Err(Error::InvalidBPSec(bpsec::Error::MissingSecurityTarget))
        ));
    }

    // RFC 9172 §5.1.1: when remove_blocks is called on a target whose
    // covering BIB is BCB-encrypted with a wrong key, the cascade
    // leniently continues past the DecryptionFailed (instead of erroring)
    // and removes the target and all named security blocks cleanly.
    //
    // This exercises the edit-level failure-drop path directly — without
    // going through `Parsed::check` — and confirms the DecryptFailed →
    // continue change in bpsec::edit::remove_blocks.
    #[test]
    fn remove_blocks_failure_drop_with_undecryptable_bib() {
        let sign_k = sign_key();
        // Bind both `k` values so the wrong-key premise is structural.
        let enc_k_val = rand_k(16);
        let wrong_k_val = rand_k(16);
        assert_ne!(
            enc_k_val, wrong_k_val,
            "the wrong key must differ from the encryption key"
        );
        let enc_k = enc_key_with_k(&enc_k_val);

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
        let wrong_enc_k = enc_key_with_k(&wrong_k_val);
        let wrong_keys = bpsec::key::KeySet::new(vec![sign_k, wrong_enc_k]);

        // §5.1.1 failure-drop at the editor level: include the corrupt
        // block and all its associated security blocks in to_remove.
        // remove_blocks hits DecryptFailed on BIB staging and continues
        // (was a hard error before this fix); all four blocks are removed.
        let to_remove: HashSet<u64> = [2, bib_num, bcb_over_2, bcb_over_bib].into_iter().collect();
        let (bundle, _chunks) = rewrite::apply_rewrites(
            Editor::new(&raw, &enc_bytes).with_key_source(&wrong_keys),
            HashMap::new(),
            to_remove,
        )
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
            .remove_blocks(&HashSet::from([bib_num]))
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
            .remove_blocks(&HashSet::from([0u64]))
            .map(|_| ())
            .unwrap_err();
        assert!(matches!(err, editor::Error::PrimaryBlock), "{err}");
        let (_, err) = Editor::new(&raw, &bytes)
            .remove_blocks(&HashSet::from([1u64]))
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
            .remove_blocks(&HashSet::from([bcb_over_2]))
            .map(|_| ())
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("without also removing all of its targets"),
            "{err}"
        );
    }

    // A `Maybe`-covered target must not be removed while its covering
    // encrypted BIB survives undecrypted, which would list a block that no
    // longer exists: the cascade refuses, naming the target, as does
    // apply_rewrites.
    #[test]
    fn maybe_covered_target_refused_without_keys() {
        let sign_k = sign_key();
        let enc_k = enc_key();
        let base = build_with_unknown_block();
        let signed = sign(&base, &[2], &sign_k);
        let encrypted = encrypt(&signed, 2, &enc_k);

        let (bytes, raw, _, _) = raw_parse_tuple(Bytes::copy_from_slice(&encrypted)).unwrap();
        assert!(matches!(raw.blocks[&2].bib, block::BibCoverage::Maybe));

        let refused = Editor::new(&raw, &bytes)
            .remove_blocks(&HashSet::from([2u64]))
            .map(|_| ())
            .map_err(|(_, e)| e);
        assert!(
            matches!(refused, Err(editor::Error::MaybeHasBib(2))),
            "{refused:?}"
        );

        let result = rewrite::apply_rewrites(
            Editor::new(&raw, &bytes),
            HashMap::new(),
            HashSet::from([2u64]),
        );
        assert!(
            matches!(result, Err(editor::Error::MaybeHasBib(2))),
            "{result:?}"
        );
    }

    // Phantom block numbers in the request are not reported as removals
    // and produce no rewrite (parse-review finding E11e).
    #[test]
    fn phantom_removals_are_not_reported() {
        let base = build_with_unknown_block();
        let (bytes, raw, _, _) = raw_parse_tuple(Bytes::copy_from_slice(&base)).unwrap();

        let (_, removed) = Editor::new(&raw, &bytes)
            .remove_blocks(&HashSet::from([99u64]))
            .map_err(|(_, e)| e)
            .unwrap();
        assert!(removed.is_empty());

        let result = rewrite::apply_rewrites(
            Editor::new(&raw, &bytes),
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

        let editor = Editor::new(&raw, &bytes)
            .with_key_source(&keys)
            .remove_encryption(1)
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

    // Keys by security source. A `KeySet` hands out its first key for an
    // operation whatever the source, so it cannot hold two encryption keys a
    // node uses for different sources.
    struct KeysBySource(Vec<(eid::Eid, bpsec::key::Key)>);

    impl bpsec::key::KeySource for KeysBySource {
        fn key<'a>(
            &'a self,
            source: &eid::Eid,
            operations: &[bpsec::key::Operation],
        ) -> Option<&'a bpsec::key::Key> {
            self.0
                .iter()
                .find(|(key_source, key)| {
                    key_source == source
                        && key
                            .operations
                            .as_ref()
                            .is_some_and(|ops| operations.iter().any(|op| ops.contains(op)))
                })
                .map(|(_, key)| key)
        }
    }

    // After a removal coupled to the BIB over blocks 2 and 3: no BCB is
    // left, the BIB is in plaintext over both, each holds its plaintext
    // with the CRC type it was made with, none, and the BIB still verifies.
    fn assert_coupled_set_decrypted(
        editor: Editor<'_>,
        bytes: &[u8],
        bib: u64,
        sign_k: &bpsec::key::Key,
    ) {
        let (bundle, chunks) = editor.rebuild_bundle().unwrap();
        for target in [2, 3] {
            assert_eq!(bundle.blocks[&target].bib, block::BibCoverage::Some(bib));
        }
        let decrypted = editor::Chunk::flatten(chunks, bytes);
        let mut parsed = parse::parse(Bytes::copy_from_slice(&decrypted)).expect("parse");
        let raw = &parsed.bundle;
        assert_eq!(count_type(raw, block::Type::BlockSecurity), 0);
        assert!(raw.blocks[&bib].bcb.is_none());
        for (target, plaintext) in [(2, [0xDE, 0xAD]), (3, [0xBE, 0xEF])] {
            let block = &raw.blocks[&target];
            assert_eq!(block.payload(&parsed.data), Some(plaintext.as_slice()));
            assert_eq!(block.crc_type, crc::CrcType::None);
            assert_eq!(block.bib, block::BibCoverage::Some(bib));
        }
        let facts = common::verify_as_receiver(
            &mut parsed,
            &bpsec::key::KeySet::new(vec![sign_k.clone()]),
            &HashSet::new(),
        )
        .expect("the bundle verifies");
        assert!(!facts.any(Checked::Failed), "the BIB verifies");
    }

    // Removing the encryption of a block an encrypted BIB covers removes the
    // BIB's encryption, and that of the BIB's other target, with it: all or
    // none (RFC 9172 §3.9). The block's coverage, read as `Maybe`, resolves
    // to the BIB.
    #[test]
    fn remove_encryption_takes_the_encrypted_bib_over_its_target() {
        let sign_k = sign_key();
        let enc_k = enc_key();
        let encrypted = encrypt(
            &sign(&build_with_unknown_blocks(), &[2, 3], &sign_k),
            2,
            &enc_k,
        );
        let (bytes, raw, _, _) =
            raw_parse_tuple(Bytes::copy_from_slice(&encrypted)).expect("parse");
        assert_eq!(raw.blocks[&2].bib, block::BibCoverage::Maybe);
        let bib = find_bib(&raw).expect("BIB present");

        let keys = bpsec::key::KeySet::new(vec![enc_k]);
        let editor = Editor::new(&raw, &bytes)
            .with_key_source(&keys)
            .remove_encryption(2)
            .map_err(|(_, e)| e)
            .unwrap();
        assert_coupled_set_decrypted(editor, &bytes, bib, &sign_k);
    }

    // Removing an encrypted BIB's encryption removes its targets' with it.
    #[test]
    fn remove_encryption_of_a_bib_takes_its_targets() {
        let sign_k = sign_key();
        let enc_k = enc_key();
        let encrypted = encrypt(
            &sign(&build_with_unknown_blocks(), &[2, 3], &sign_k),
            2,
            &enc_k,
        );
        let (bytes, raw, _, _) =
            raw_parse_tuple(Bytes::copy_from_slice(&encrypted)).expect("parse");
        let bib = find_bib(&raw).expect("BIB present");

        let keys = bpsec::key::KeySet::new(vec![enc_k]);
        let editor = Editor::new(&raw, &bytes)
            .with_key_source(&keys)
            .remove_encryption(bib)
            .map_err(|(_, e)| e)
            .unwrap();
        assert_coupled_set_decrypted(editor, &bytes, bib, &sign_k);
    }

    // A removal from supplied plaintext that lacks a block the coupling
    // needs, with no key to decrypt it, removes nothing; supplied whole, it
    // needs no key.
    #[test]
    fn remove_encryption_from_supplied_plaintext_is_all_or_none() {
        let sign_k = sign_key();
        let signed = sign(&build_with_unknown_blocks(), &[2, 3], &sign_k);
        let encrypted = encrypt(&signed, 2, &enc_key());
        let (signed_bytes, signed_raw, _, _) =
            raw_parse_tuple(Bytes::copy_from_slice(&signed)).expect("parse");
        let bib = find_bib(&signed_raw).expect("BIB present");
        let bib_plaintext = signed_raw.blocks[&bib]
            .payload(&signed_bytes)
            .expect("BIB body in bundle");
        let plaintext = |bytes: &[u8]| Zeroizing::new(Box::from(bytes));
        let partial = Decrypted {
            plaintext: HashMap::from([
                (2, plaintext(&[0xDE, 0xAD])),
                (bib, plaintext(bib_plaintext)),
            ]),
            ..Default::default()
        };
        let mut complete = Decrypted {
            plaintext: partial.plaintext.clone(),
            ..Default::default()
        };
        complete.plaintext.insert(3, plaintext(&[0xBE, 0xEF]));

        let (bytes, raw, _, _) =
            raw_parse_tuple(Bytes::copy_from_slice(&encrypted)).expect("parse");
        let no_keys = empty_keys();
        let Err((editor, e)) = Editor::new(&raw, &bytes)
            .with_key_source(&no_keys)
            .with_decrypted(&partial)
            .remove_encryption(2)
        else {
            panic!("block 3's plaintext is neither supplied nor decryptable");
        };
        assert!(matches!(e, editor::Error::Bpsec(bpsec::Error::NoKey)));
        assert_eq!(
            editor::Chunk::flatten(editor.rebuild().unwrap(), &bytes),
            encrypted,
            "nothing is removed"
        );

        let editor = Editor::new(&raw, &bytes)
            .with_key_source(&no_keys)
            .with_decrypted(&complete)
            .remove_encryption(2)
            .map_err(|(_, e)| e)
            .unwrap();
        assert_coupled_set_decrypted(editor, &bytes, bib, &sign_k);
    }

    // A block whose coverage reads `Maybe` is resolved by decrypting the
    // bundle's encrypted BIBs. One this node cannot decrypt leaves the
    // coverage open, so the removal fails with that BIB's error; once the
    // BIB decrypts and does not list the block, the block alone is
    // decrypted, with the CRC type it was made with, none.
    #[test]
    fn remove_encryption_resolves_maybe_coverage() {
        let key_a = enc_key();
        let key_b = enc_key();
        // Block 3 is encrypted under ipn:3.1 with no BIB over it; block 2 is
        // signed, then encrypted with its BIB under ipn:2.1.
        let encrypted = encrypt(
            &sign(
                &encrypt_as(&build_with_unknown_blocks(), 3, "ipn:3.1", &key_b),
                &[2],
                &sign_key(),
            ),
            2,
            &key_a,
        );
        let (bytes, raw, _, _) =
            raw_parse_tuple(Bytes::copy_from_slice(&encrypted)).expect("parse");
        assert_eq!(raw.blocks[&3].bib, block::BibCoverage::Maybe);
        let bib = find_bib(&raw).expect("BIB present");
        let source_a: eid::Eid = "ipn:2.1".parse().unwrap();
        let source_b: eid::Eid = "ipn:3.1".parse().unwrap();

        let only_b = KeysBySource(vec![(source_b.clone(), key_b.clone())]);
        let Err((_, e)) = Editor::new(&raw, &bytes)
            .with_key_source(&only_b)
            .remove_encryption(3)
        else {
            panic!("the BIB this node cannot decrypt may cover block 3");
        };
        assert!(matches!(e, editor::Error::Bpsec(bpsec::Error::NoKey)));

        let both = KeysBySource(vec![(source_a, key_a), (source_b, key_b)]);
        let editor = Editor::new(&raw, &bytes)
            .with_key_source(&both)
            .remove_encryption(3)
            .map_err(|(_, e)| e)
            .unwrap();
        let (bundle, chunks) = editor.rebuild_bundle().unwrap();
        assert_eq!(bundle.blocks[&3].bib, block::BibCoverage::None);
        let decrypted = editor::Chunk::flatten(chunks, &bytes);
        let (data, raw, _, _) = raw_parse_tuple(Bytes::copy_from_slice(&decrypted)).expect("parse");
        let block = &raw.blocks[&3];
        assert_eq!(block.payload(&data), Some([0xBE, 0xEF].as_slice()));
        assert!(block.bcb.is_none());
        assert_eq!(block.crc_type, crc::CrcType::None);
        assert!(raw.blocks[&bib].bcb.is_some(), "the BIB stays encrypted");
        assert!(raw.blocks[&2].bcb.is_some(), "its target stays encrypted");
    }

    // No security operation changes a target's CRC: removing a BIB
    // operation leaves its target with the CRC type it had, none for a
    // target signed without one, under BIB-HMAC-SHA2 as under an
    // unrecognised context, and its CRC for a target signed with one, which
    // verification accepts as it stands.
    #[test]
    fn removing_a_bib_operation_leaves_the_targets_crc_as_it_is() {
        let signed = sign(&build_with_unknown_block(), &[2], &sign_key());
        let (data, raw, _, _) = raw_parse_tuple(Bytes::copy_from_slice(&signed)).expect("parse");
        assert_eq!(raw.blocks[&2].crc_type, crc::CrcType::None);
        let (bundle, _) = Editor::new(&raw, &data)
            .remove_integrity(2)
            .map_err(|(_, e)| e)
            .unwrap()
            .rebuild_bundle()
            .unwrap();
        assert_eq!(bundle.blocks[&2].crc_type, crc::CrcType::None);

        let key = sign_key();
        let signed = sign(&build_with_crc_block(), &[2], &key);
        let mut parsed = parse::parse(Bytes::copy_from_slice(&signed)).expect("parse");
        assert_eq!(
            parsed.bundle.blocks[&2].crc_type,
            crc::CrcType::CRC32_CASTAGNOLI,
            "signing leaves the target's CRC"
        );
        let facts = common::verify_as_receiver(
            &mut parsed,
            &bpsec::key::KeySet::new(vec![key]),
            &HashSet::new(),
        )
        .expect("the bundle verifies");
        assert!(
            found_targets(&facts, Checked::Verified).contains(&2),
            "the operation verifies over a target that carries a CRC"
        );
        let (bundle, _) = Editor::new(&parsed.bundle, &parsed.data)
            .remove_integrity(2)
            .map_err(|(_, e)| e)
            .unwrap()
            .rebuild_bundle()
            .unwrap();
        assert_eq!(bundle.blocks[&2].crc_type, crc::CrcType::CRC32_CASTAGNOLI);

        let (_, base) =
            builder::Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
                .with_payload(b"payload data".as_slice().into())
                .build(creation_timestamp::CreationTimestamp::now())
                .unwrap();
        let unrecognised = insert_after_primary(
            &base,
            &[
                &make_block(999, 2, 0, &[0xDE, 0xAD]),
                &make_block(11, 3, 0, &make_unknown_context_asb(2)),
            ],
        );
        let (data, raw, _, _) = raw_parse_tuple(Bytes::from(unrecognised)).expect("parse");
        assert_eq!(raw.blocks[&2].bib, block::BibCoverage::Some(3));
        let (bundle, _) = Editor::new(&raw, &data)
            .remove_integrity(2)
            .map_err(|(_, e)| e)
            .unwrap()
            .rebuild_bundle()
            .unwrap();
        assert_eq!(bundle.blocks[&2].crc_type, crc::CrcType::None);
        assert!(!bundle.blocks.contains_key(&3), "the emptied BIB goes");
    }

    // Likewise for a BCB operation: a target BCB-AES-GCM decrypts, or one
    // accepted from supplied plaintext under an unrecognised context, keeps
    // the CRC type it had, none; a target encrypted with a CRC carries it
    // over its ciphertext, decrypts, and keeps it over its plaintext.
    #[test]
    fn removing_a_bcb_operation_leaves_the_targets_crc_as_it_is() {
        let plaintexts = Decrypted {
            plaintext: HashMap::from([(2, Zeroizing::new(Box::from([0xDE, 0xAD].as_slice())))]),
            ..Default::default()
        };
        let no_keys = empty_keys();

        let encrypted = encrypt(&build_with_unknown_block(), 2, &enc_key());
        let (data, raw, _, _) = raw_parse_tuple(Bytes::copy_from_slice(&encrypted)).expect("parse");
        assert_eq!(raw.blocks[&2].crc_type, crc::CrcType::None);
        let (bundle, _) = Editor::new(&raw, &data)
            .with_key_source(&no_keys)
            .with_decrypted(&plaintexts)
            .remove_encryption(2)
            .map_err(|(_, e)| e)
            .unwrap()
            .rebuild_bundle()
            .unwrap();
        assert_eq!(bundle.blocks[&2].crc_type, crc::CrcType::None);

        let enc = enc_key();
        let encrypted = encrypt(&build_with_crc_block(), 2, &enc);
        let (data, raw, _, _) = raw_parse_tuple(Bytes::copy_from_slice(&encrypted)).expect("parse");
        assert_eq!(
            raw.blocks[&2].crc_type,
            crc::CrcType::CRC32_CASTAGNOLI,
            "encrypting leaves the target's CRC type, the CRC over its ciphertext"
        );
        assert!(raw.blocks[&2].bcb.is_some());
        let (bundle, chunks) = Editor::new(&raw, &data)
            .with_key_source(&bpsec::key::KeySet::new(vec![enc]))
            .remove_encryption(2)
            .map_err(|(_, e)| e)
            .unwrap()
            .rebuild_bundle()
            .unwrap();
        assert_eq!(bundle.blocks[&2].crc_type, crc::CrcType::CRC32_CASTAGNOLI);
        let decrypted = editor::Chunk::flatten(chunks, &data);
        let (data, raw, _, _) = raw_parse_tuple(Bytes::copy_from_slice(&decrypted))
            .expect("the CRC is the plaintext's");
        assert_eq!(raw.blocks[&2].payload(&data), Some([0xDE, 0xAD].as_slice()));
        assert!(raw.blocks[&2].bcb.is_none());

        let (_, base) =
            builder::Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
                .with_payload(b"payload data".as_slice().into())
                .build(creation_timestamp::CreationTimestamp::now())
                .unwrap();
        let unrecognised = insert_after_primary(
            &base,
            &[
                &make_block(999, 2, 0, &[0xDE, 0xAD]),
                &make_block(12, 3, 0, &make_unknown_context_asb(2)),
            ],
        );
        let (data, raw, _, _) = raw_parse_tuple(Bytes::from(unrecognised)).expect("parse");
        assert_eq!(raw.blocks[&2].bcb, Some(3));
        let (bundle, _) = Editor::new(&raw, &data)
            .with_key_source(&no_keys)
            .with_decrypted(&plaintexts)
            .remove_encryption(2)
            .map_err(|(_, e)| e)
            .unwrap()
            .rebuild_bundle()
            .unwrap();
        assert_eq!(bundle.blocks[&2].crc_type, crc::CrcType::None);
        assert!(!bundle.blocks.contains_key(&3), "the emptied BCB goes");
    }

    // A target's CRC forbids nothing: a target that carries a CRC under an
    // unrecognised context's BCB fails to decrypt for want of the context,
    // not for its CRC.
    #[test]
    fn an_unrecognised_bcb_context_does_not_forbid_the_targets_crc() {
        let (_, base) =
            builder::Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
                .add_extension_block(block::Type::Unrecognised(999))
                .unwrap()
                .with_crc_type(crc::CrcType::CRC32_CASTAGNOLI)
                .build([0xDE, 0xAD].as_slice().into())
                .with_payload(b"payload data".as_slice().into())
                .build(creation_timestamp::CreationTimestamp::now())
                .unwrap();
        let encrypted = insert_after_primary(
            &base,
            &[&make_block(12, 3, 0, &make_unknown_context_asb(2))],
        );
        let (data, raw, _, _) = raw_parse_tuple(Bytes::from(encrypted)).expect("parse");
        assert_eq!(raw.blocks[&2].crc_type, crc::CrcType::CRC32_CASTAGNOLI);
        assert_eq!(raw.blocks[&2].bcb, Some(3));

        assert!(matches!(
            Editor::new(&raw, &data).remove_encryption(2),
            Err((
                _,
                editor::Error::Bpsec(bpsec::Error::UnrecognisedContext(99))
            ))
        ));
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
// On a headers-only buffer (payload not yet drained), `check` can't
// check a BIB that targets the payload, so it records that operation
// `Deferred`, leaving `bib_ops` whole; the gate begins a streaming verifier
// per deferred operation (`bib::Operation::begin_verify`) and feeds it the
// payload as it streams. An encrypted payload is decrypted
// first, the same way (`bcb::Operation::begin_decrypt`).
#[cfg(all(feature = "rfc9173", feature = "serde"))]
mod deferred_payload_bib_tests {
    use super::*;
    use hardy_bpv7::parse::{BundleParser, ParserProgress};

    // The streaming verifier of each BIB in `bib_ops` over the payload, fed
    // `target_len` bytes, against `data`, which need not hold the payload: a
    // missing key begins none.
    fn payload_verifiers(
        data: &[u8],
        keys: &bpsec::key::KeySet,
        blocks: &HashMap<u64, block::Block>,
        bib_ops: &HashMap<u64, bpsec::bib::OperationSet>,
        target_len: u64,
    ) -> Vec<(u64, bpsec::bib::Verifier)> {
        let reader = PlainReader {
            blocks,
            source_data: data,
        };
        bib_ops
            .iter()
            .filter_map(|(&bib, ops)| {
                let op = ops.operations().get(&1)?;
                let args = bpsec::bib::OperationArgs {
                    source: ops.source(),
                    target: 1,
                    security_block: bib,
                    blocks: &reader,
                };
                match op.begin_verify(keys, &args, target_len) {
                    Ok(verifier) => Some((bib, verifier)),
                    Err(bpsec::Error::NoKey) => None,
                    Err(e) => panic!("BIB {bib}: {e}"),
                }
            })
            .collect()
    }

    // The payload's received length, from its parsed extent.
    fn received_len(blocks: &HashMap<u64, block::Block>) -> u64 {
        let data = &blocks[&1].data;
        data.end - data.start
    }

    // Begin decrypting the payload under the BCB that covers it, against
    // `data`, which need not hold the payload.
    fn payload_decryptor(
        data: &[u8],
        keys: &bpsec::key::KeySet,
        blocks: &HashMap<u64, block::Block>,
        bcb_ops: &HashMap<u64, bpsec::bcb::OperationSet>,
    ) -> Result<bpsec::bcb::Decryptor, bpsec::Error> {
        let bcb = blocks[&1].bcb.expect("a BCB covers the payload");
        let ops = &bcb_ops[&bcb];
        ops.operations()[&1].begin_decrypt(
            keys,
            &bpsec::bcb::OperationArgs {
                source: ops.source(),
                target: 1,
                security_block: bcb,
                blocks: &PlainReader {
                    blocks,
                    source_data: data,
                },
            },
            received_len(blocks),
        )
    }

    fn sign_key() -> bpsec::key::Key {
        serde_json::from_value(serde_json::json!({
            "kid": "ipn:2.1",
            "kty": "oct",
            "alg": "HS256",
            "key_ops": ["sign", "verify"],
            "k": rand_k(18)
        }))
        .unwrap()
    }

    // A bundle whose payload (block 1) is signed under a BIB and is far larger
    // than any sane parser chunk, so the streaming parser must report `Partial`
    // before the payload body is resident.
    fn signed_large_payload(key: &bpsec::key::Key) -> Box<[u8]> {
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
                key,
            )
            .map_err(|(_, e)| e)
            .unwrap()
            .rebuild()
            .unwrap()
    }

    // Bind the key once; the signed bundle and the verifying KeySet derive
    // from the same binding.
    fn signed_payload_and_keys() -> (Box<[u8]>, bpsec::key::KeySet) {
        let key = sign_key();
        let full = signed_large_payload(&key);
        (full, bpsec::key::KeySet::new(vec![key]))
    }

    // Drive the streaming parser in 64-byte pushes until it hands back the
    // payload's tail, returning the parsed headers — block 1's extent
    // over-claims, its body isn't resident in `parsed.data`.
    fn parse_headers_only(full: &[u8]) -> Parsed {
        let mut parser = BundleParser::new(256);
        for c in full.chunks(64) {
            match parser.push(Bytes::copy_from_slice(c)).unwrap() {
                ParserProgress::NeedMore(_) => {}
                ParserProgress::Partial { consumed, .. } => {
                    return parser.finish(consumed).unwrap();
                }
                ParserProgress::Ready(_) => {
                    panic!("a bundle pushed in pieces must reach Partial, not Ready")
                }
            }
        }
        panic!("parser never reached Partial");
    }

    // Header pass defers the block-1 BIB (payload not resident), handing its
    // op-set over owned; the map then verifies against the full bundle.
    #[test]
    fn payload_bib_deferred_then_verified() {
        let (full, keys) = signed_payload_and_keys();

        let mut parsed = parse_headers_only(&full);
        let bib_block = *parsed.bibs.keys().next().expect("a BIB op-set");
        assert!(
            parsed.bundle.blocks[&1].payload(&parsed.data).is_none(),
            "payload body must not be resident in the headers-only buffer"
        );

        let facts = common::verify_as_receiver(&mut parsed, &keys, &HashSet::new()).unwrap();
        assert_eq!(
            found(&facts, Checked::Deferred),
            vec![(bib_block, 1)],
            "the block-1 BIB is deferred, not checked inline"
        );
        assert!(
            parsed.bibs.contains_key(&bib_block),
            "verify leaves the BIB operation sets whole"
        );

        // The gate feeds each deferred verifier the now-resident payload and
        // settles it — the streaming drain does the same, segment by segment.
        // (A tampered-payload failure is the `_incremental_tamper_fails` twin
        // below.)
        let raw = &parsed.bundle;
        for (_, mut verifier) in payload_verifiers(
            &full,
            &keys,
            &raw.blocks,
            &parsed.bibs,
            received_len(&raw.blocks),
        ) {
            let payload = raw.blocks[&1].payload(&full).unwrap();
            verifier.update(payload);
            verifier
                .finish()
                .expect("deferred payload BIB verifies against the full bundle");
        }
    }

    // Run the header pass on the headers-only buffer and hand back the
    // pieces the streaming-verifier tests need: the consumed prefix, the
    // structural bundle, its BIB op-sets, and the outcomes.
    fn deferred_setup(
        full: &[u8],
        keys: &bpsec::key::KeySet,
    ) -> (
        Bytes,
        hardy_bpv7::bundle::Bundle,
        HashMap<u64, bpsec::bib::OperationSet>,
        checks::Facts,
    ) {
        let mut parsed = parse_headers_only(full);
        // As the payload's acceptor: an encrypted BIB that may cover it is
        // decrypted, so its op-set is handed over for the plaintext.
        let facts = common::verify_as_receiver(&mut parsed, keys, &HashSet::from([1])).unwrap();
        let Parsed {
            data: consumed,
            bundle: raw,
            bibs: bib_ops,
            ..
        } = parsed;
        (consumed, raw, bib_ops, facts)
    }

    // The streamed twin of `payload_bib_deferred_then_verified`: the
    // deferred op-set becomes an incremental verifier constructed from the
    // headers-only buffer, fed the payload's block-type-specific data in
    // awkward chunk sizes, and settles Ok — no resident payload anywhere.
    #[test]
    fn payload_bib_verifies_incrementally() {
        let (full, keys) = signed_payload_and_keys();
        let (consumed, raw, bibs, facts) = deferred_setup(&full, &keys);
        let bib_block = found(&facts, Checked::Deferred)
            .first()
            .expect("a deferred BIB")
            .0;

        let mut verifiers = payload_verifiers(
            &consumed,
            &keys,
            &raw.blocks,
            &bibs,
            received_len(&raw.blocks),
        );
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
        let (full, keys) = signed_payload_and_keys();
        let (consumed, raw, bibs, _) = deferred_setup(&full, &keys);

        let mut verifiers = payload_verifiers(
            &consumed,
            &keys,
            &raw.blocks,
            &bibs,
            received_len(&raw.blocks),
        );
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

    // A BIB operation checked without its key is reported, not skipped
    // silently: a verifier's policy skip, an acceptor's failed acceptance.
    #[test]
    fn a_bib_without_its_key_is_reported() {
        let (full, keys) = signed_payload_and_keys();
        let bib_block = *parse::parse(Bytes::copy_from_slice(&full))
            .unwrap()
            .bibs
            .keys()
            .next()
            .expect("a BIB op-set");
        let mut parsed = parse::parse(Bytes::copy_from_slice(&full)).unwrap();
        let facts = common::verify_as_receiver(
            &mut parsed,
            &bpsec::key::KeySet::new(vec![]),
            &HashSet::new(),
        )
        .expect("a missing key is a fact, not an error");
        assert_eq!(found(&facts, Checked::NoKey), vec![(bib_block, 1)]);
        assert!(!facts.any(Checked::Failed));

        // With the key, the same operation passes.
        let mut parsed = parse::parse(Bytes::copy_from_slice(&full)).unwrap();
        let facts = common::verify_as_receiver(&mut parsed, &keys, &HashSet::new()).unwrap();
        assert!(!facts.any(Checked::NoKey) && !facts.any(Checked::Failed));
    }

    // No usable key is a soft policy skip: construction yields no verifiers
    // rather than an error.
    #[test]
    fn payload_bib_incremental_nokey_skips() {
        let (full, keys) = signed_payload_and_keys();
        let (consumed, raw, bibs, _) = deferred_setup(&full, &keys);

        let empty_keys = bpsec::key::KeySet::new(vec![]);
        let verifiers = payload_verifiers(
            &consumed,
            &empty_keys,
            &raw.blocks,
            &bibs,
            received_len(&raw.blocks),
        );
        assert!(verifiers.is_empty());
    }

    // The verifier must be able to cross await points and task boundaries:
    // it owns the keyed MAC state the drain feeds.
    #[test]
    fn bib_verifier_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<bpsec::bib::Verifier>();
    }

    // On the headers-only buffer the payload's operation is deferred, as an
    // outcome naming the BIB and the payload, not checked and not failed.
    #[test]
    fn verify_defers_the_payloads_operation_on_a_headers_only_buffer() {
        let (full, keys) = signed_payload_and_keys();

        let mut parsed = parse_headers_only(&full);
        let bib_block = *parsed.bibs.keys().next().expect("a BIB op-set");

        let facts = common::verify_as_receiver(&mut parsed, &keys, &HashSet::new())
            .expect("the non-resident payload target defers, it does not fail");
        assert_eq!(found(&facts, Checked::Deferred), vec![(bib_block, 1)]);
        assert!(!facts.any(Checked::Failed));
    }

    // With the whole bundle resident, `check` verifies the payload BIB inline and
    // defers nothing — the non-streaming path is unchanged.
    #[test]
    fn all_resident_verifies_inline_without_deferring() {
        let (full, keys) = signed_payload_and_keys();

        let mut parsed = parse::parse(Bytes::copy_from_slice(&full)).unwrap();
        let facts = common::verify_as_receiver(&mut parsed, &keys, &HashSet::new()).unwrap();
        assert!(
            !facts.any(Checked::Deferred),
            "nothing is deferred when the payload is resident"
        );
    }

    fn enc_key() -> bpsec::key::Key {
        serde_json::from_value(serde_json::json!({
            "kid": "ipn:2.1",
            "kty": "oct",
            "alg": "A128KW",
            "enc": "A128GCM",
            "key_ops": ["encrypt", "decrypt", "wrapKey", "unwrapKey"],
            "k": rand_k(16)
        }))
        .unwrap()
    }

    // `signed_large_payload`'s bundle with its payload then encrypted, which
    // encrypts the BIB over it too (RFC 9172 §3.9).
    fn signed_then_encrypted_payload(
        sign_key: &bpsec::key::Key,
        enc_key: &bpsec::key::Key,
    ) -> Box<[u8]> {
        let signed = parse::parse(Bytes::from(signed_large_payload(sign_key))).unwrap();
        bpsec::encryptor::Encryptor::new(&signed.bundle, &signed.data)
            .encrypt_block(
                1,
                bpsec::encryptor::Context::AES_GCM(bpsec::rfc9173::ScopeFlags {
                    include_security_header: false,
                    ..Default::default()
                }),
                "ipn:2.1".parse().unwrap(),
                enc_key,
            )
            .map_err(|(_, e)| e)
            .unwrap()
            .rebuild()
            .unwrap()
    }

    // The payload's plaintext, decrypted from the whole bundle.
    fn payload_plaintext(full: &[u8], keys: &bpsec::key::KeySet) -> Box<[u8]> {
        let parsed = parse::parse(Bytes::copy_from_slice(full)).unwrap();
        let reader =
            bpsec::DecryptingReader::new(&parsed.bundle.blocks, &parsed.data, &parsed.bcbs, keys);
        let payload = reader
            .into_block_data(1)
            .expect("the payload decrypts")
            .expect("the payload is resident");
        Box::from(payload.as_ref())
    }

    // An encrypted payload's BIB cannot be checked against the ciphertext
    // (RFC 9172 §3.9), so the header pass hands it over for the payload's
    // acceptor; begun with the plaintext's length and fed the plaintext, it
    // verifies.
    #[test]
    fn an_encrypted_payloads_bib_verifies_against_its_plaintext() {
        let sign_key = sign_key();
        let enc_key = enc_key();
        let full = signed_then_encrypted_payload(&sign_key, &enc_key);
        let keys = bpsec::key::KeySet::new(vec![sign_key, enc_key]);
        let (consumed, raw, bibs, facts) = deferred_setup(&full, &keys);

        assert_eq!(
            facts
                .targets_of(SecurityService::Integrity, Checked::Deferred)
                .count(),
            1,
            "the operation on the encrypted payload is deferred to its acceptor"
        );
        let plaintext = payload_plaintext(&full, &keys);
        let verifiers =
            payload_verifiers(&consumed, &keys, &raw.blocks, &bibs, plaintext.len() as u64);
        assert_eq!(verifiers.len(), 1);
        for (_, mut verifier) in verifiers {
            for chunk in plaintext.chunks(4096) {
                verifier.update(chunk);
            }
            verifier
                .finish()
                .expect("the BIB verifies against the plaintext");
        }
    }

    // The same verifier fed altered plaintext fails the integrity check.
    #[test]
    fn an_encrypted_payloads_bib_rejects_altered_plaintext() {
        let sign_key = sign_key();
        let enc_key = enc_key();
        let full = signed_then_encrypted_payload(&sign_key, &enc_key);
        let keys = bpsec::key::KeySet::new(vec![sign_key, enc_key]);
        let (consumed, raw, bibs, _) = deferred_setup(&full, &keys);

        let mut plaintext = payload_plaintext(&full, &keys).into_vec();
        let mid = plaintext.len() / 2;
        plaintext[mid] ^= 0xFF;
        let (_, mut verifier) =
            payload_verifiers(&consumed, &keys, &raw.blocks, &bibs, plaintext.len() as u64)
                .pop()
                .expect("one verifier");
        verifier.update(&plaintext);
        assert!(matches!(
            verifier.finish(),
            Err(bpsec::Error::IntegrityCheckFailed)
        ));
    }

    // An acceptor learns it holds no key for an operation without
    // processing it: each operation finds its key as its own processing
    // would, and a key for another operation does not count.
    #[test]
    fn require_key_finds_the_key_each_operation_needs() {
        let sign_key = sign_key();
        let enc_key = enc_key();
        let full = signed_then_encrypted_payload(&sign_key, &enc_key);
        let both = bpsec::key::KeySet::new(vec![sign_key.clone(), enc_key.clone()]);
        let (_, raw, bibs, _) = deferred_setup(&full, &both);
        let parsed = parse::parse(Bytes::copy_from_slice(&full)).unwrap();

        let bcb_number = raw
            .blocks
            .get(&1)
            .unwrap()
            .bcb
            .expect("the payload is encrypted");
        let bcb_ops = parsed.bcbs.get(&bcb_number).expect("its BCB");
        let decrypt = bcb_ops
            .operations()
            .get(&1)
            .expect("the payload's BCB operation");
        let bib_ops = bibs.values().next().expect("the payload's BIB");
        let verify = bib_ops
            .operations()
            .get(&1)
            .expect("the payload's BIB operation");

        let only_sign = bpsec::key::KeySet::new(vec![sign_key]);
        let only_enc = bpsec::key::KeySet::new(vec![enc_key]);
        let none = bpsec::key::KeySet::new(vec![]);

        decrypt
            .require_key(&only_enc, bcb_ops.source())
            .expect("the encryption key serves the BCB operation");
        assert!(matches!(
            decrypt.require_key(&only_sign, bcb_ops.source()),
            Err(bpsec::Error::NoKey)
        ));
        assert!(matches!(
            decrypt.require_key(&none, bcb_ops.source()),
            Err(bpsec::Error::NoKey)
        ));
        verify
            .require_key(&only_sign, bib_ops.source())
            .expect("the signing key serves the BIB operation");
        assert!(matches!(
            verify.require_key(&only_enc, bib_ops.source()),
            Err(bpsec::Error::NoKey)
        ));
        assert!(matches!(
            verify.require_key(&none, bib_ops.source()),
            Err(bpsec::Error::NoKey)
        ));
    }

    // A direct-mode content-encryption key: its value is immaterial, so it is
    // generated, and its bytes are reachable for the redaction check.
    fn direct_enc_key() -> bpsec::key::Key {
        serde_json::from_value(serde_json::json!({
            "kid": "ipn:2.1",
            "kty": "oct",
            "alg": "dir",
            "enc": "A256GCM",
            "key_ops": ["encrypt", "decrypt"],
            "k": rand_k(32)
        }))
        .unwrap()
    }

    // The payload's received data: its ciphertext, Hardy's encryptor
    // attaching the tag to it (RFC 9173 §4.4).
    fn received_payload<'a>(raw: &hardy_bpv7::bundle::Bundle, full: &'a [u8]) -> &'a [u8] {
        raw.blocks[&1]
            .payload(full)
            .expect("the full bundle holds the payload")
    }

    // The payload's acceptor decrypts it as it streams, from a headers-only
    // buffer: fed the received data in awkward runs, the decryptor hands
    // back the plaintext, as long as it said, and the BIB over the payload
    // verifies against it (RFC 9172 §3.9, §5.1.1).
    #[test]
    fn the_payloads_decryptor_yields_the_plaintext_its_bib_verifies() {
        let sign_key = sign_key();
        let enc_key = enc_key();
        let full = signed_then_encrypted_payload(&sign_key, &enc_key);
        let keys = bpsec::key::KeySet::new(vec![sign_key, enc_key]);
        let parsed = parse_headers_only(&full);
        let (consumed, raw, bibs, _) = deferred_setup(&full, &keys);

        let mut decryptor = payload_decryptor(&consumed, &keys, &raw.blocks, &parsed.bcbs)
            .expect("the decryption begins on the headers alone");
        let received = received_payload(&raw, &full);
        assert_eq!(
            decryptor.plaintext_len(),
            received.len() as u64 - 16,
            "the attached tag is not plaintext"
        );
        let plaintext_len = decryptor.plaintext_len();
        for run in received.chunks(4093) {
            decryptor.update(run);
        }
        let plaintext = decryptor.finish().expect("the payload decrypts");
        assert_eq!(plaintext.len() as u64, plaintext_len);
        assert_eq!(plaintext.as_ref(), payload_plaintext(&full, &keys).as_ref());

        let verifiers = payload_verifiers(&consumed, &keys, &raw.blocks, &bibs, plaintext_len);
        assert_eq!(verifiers.len(), 1);
        for (_, mut verifier) in verifiers {
            verifier.update(&plaintext);
            verifier
                .finish()
                .expect("the BIB verifies against the decrypted payload");
        }
    }

    // A flipped byte of the received ciphertext fails to authenticate, and
    // no plaintext is released.
    #[test]
    fn a_tampered_payload_fails_to_decrypt() {
        let enc_key = enc_key();
        let full = signed_then_encrypted_payload(&sign_key(), &enc_key);
        let keys = bpsec::key::KeySet::new(vec![enc_key]);
        let parsed = parse_headers_only(&full);
        let mut received = received_payload(&parsed.bundle, &full).to_vec();
        let mid = received.len() / 2;
        received[mid] ^= 0xFF;

        let mut decryptor =
            payload_decryptor(&parsed.data, &keys, &parsed.bundle.blocks, &parsed.bcbs).unwrap();
        decryptor.update(&received);
        assert!(matches!(
            decryptor.finish(),
            Err(bpsec::Error::DecryptionFailed)
        ));
    }

    // Data short of the target's length, or past it, fails the decryption.
    #[test]
    fn a_payload_fed_short_or_long_fails_to_decrypt() {
        let enc_key = enc_key();
        let full = signed_then_encrypted_payload(&sign_key(), &enc_key);
        let keys = bpsec::key::KeySet::new(vec![enc_key]);
        let parsed = parse_headers_only(&full);
        let received = received_payload(&parsed.bundle, &full);
        let begin =
            || payload_decryptor(&parsed.data, &keys, &parsed.bundle.blocks, &parsed.bcbs).unwrap();

        let mut short = begin();
        short.update(&received[..received.len() - 1]);
        assert!(matches!(
            short.finish(),
            Err(bpsec::Error::DecryptionFailed)
        ));

        let mut long = begin();
        long.update(received);
        long.update(&[0]);
        assert!(matches!(long.finish(), Err(bpsec::Error::DecryptionFailed)));
    }

    // The key is resolved before the first byte: without it the decryption
    // does not begin (NoKey), and in key-wrap mode a key that cannot unwrap
    // the content-encryption key fails there too.
    #[test]
    fn the_payloads_decryption_needs_its_key_before_the_first_byte() {
        let sign_key = sign_key();
        let full = signed_then_encrypted_payload(&sign_key, &enc_key());
        let parsed = parse_headers_only(&full);
        let begin = |keys: &bpsec::key::KeySet| {
            payload_decryptor(&parsed.data, keys, &parsed.bundle.blocks, &parsed.bcbs)
        };

        assert!(matches!(
            begin(&bpsec::key::KeySet::new(vec![sign_key])),
            Err(bpsec::Error::NoKey)
        ));
        let other = enc_key();
        assert!(matches!(
            begin(&bpsec::key::KeySet::new(vec![other])),
            Err(bpsec::Error::DecryptionFailed)
        ));
    }

    // The decryptor holds the content-encryption key across the stream; its
    // `Debug` shows lengths, never the key.
    #[test]
    fn the_decryptors_debug_shows_no_key() {
        let enc_key = direct_enc_key();
        let bpsec::key::Type::OctetSequence { key: secret } = &enc_key.key_type else {
            panic!("an octet-sequence key");
        };
        let key_bytes = format!("{:?}", secret.expose_secret());
        let full = signed_then_encrypted_payload(&sign_key(), &enc_key);
        let keys = bpsec::key::KeySet::new(vec![enc_key.clone()]);
        let parsed = parse_headers_only(&full);
        let decryptor =
            payload_decryptor(&parsed.data, &keys, &parsed.bundle.blocks, &parsed.bcbs).unwrap();
        let shown = format!("{decryptor:?}");
        assert!(shown.contains("plaintext_len"), "{shown}");
        assert!(!shown.contains(&key_bytes), "{shown}");
        let mut decryptor = decryptor;
        decryptor.update(received_payload(&parsed.bundle, &full));
        decryptor
            .finish()
            .expect("a direct key decrypts the payload");
    }

    #[test]
    fn the_decryptor_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<bpsec::bcb::Decryptor>();
    }

    // A primary-block target's integrity input is its canonical form, not
    // streamed data, so its streaming verification does not begin: it is
    // verified resident.
    #[test]
    fn a_primary_blocks_bib_does_not_stream() {
        let key = sign_key();
        let (_, base) =
            builder::Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
                .with_payload(b"payload".as_slice().into())
                .build(creation_timestamp::CreationTimestamp::now())
                .unwrap();
        let (bytes, raw, _, _) = raw_parse_tuple(Bytes::copy_from_slice(&base)).unwrap();
        let signed = bpsec::signer::Signer::new(&raw, &bytes)
            .sign_block(
                0,
                bpsec::signer::Context::HMAC_SHA2(bpsec::rfc9173::ScopeFlags::default()),
                "ipn:2.1".parse().unwrap(),
                &key,
            )
            .map_err(|(_, e)| e)
            .unwrap()
            .rebuild()
            .unwrap();
        let parsed = parse::parse(Bytes::from(signed)).unwrap();
        let block::BibCoverage::Some(bib) = parsed.bundle.blocks[&0].bib else {
            panic!("the primary is signed");
        };
        let ops = &parsed.bibs[&bib];
        let keys = bpsec::key::KeySet::new(vec![key]);
        assert!(matches!(
            ops.operations()[&0].begin_verify(
                &keys,
                &bpsec::bib::OperationArgs {
                    source: ops.source(),
                    target: 0,
                    security_block: bib,
                    blocks: &PlainReader {
                        blocks: &parsed.bundle.blocks,
                        source_data: &parsed.data,
                    },
                },
                0,
            ),
            Err(bpsec::Error::InvalidBIBTarget)
        ));
    }
}

// A bundle from ipn:1.3.1 whose `block_type` block its source signed then
// encrypted, so the BIB over that block is encrypted too; with
// `encrypt_payload` the payload is encrypted as well, and the structure
// alone no longer shows which block the encrypted BIB covers. Returns the
// keyless parse, the signed block's number and the encryption key.
fn signed_then_encrypted(
    block_type: block::Type,
    encrypt_payload: bool,
) -> (Parsed, u64, bpsec::key::Key) {
    use bpsec::{encryptor, rfc9173::ScopeFlags, signer};
    use core::num::NonZeroU8;
    use hardy_bpv7::hop_info::HopInfo;
    use hardy_cbor::encode::emit;

    let source: eid::Eid = "ipn:1.3.1".parse().unwrap();
    let body = if block_type == block::Type::PreviousNode {
        // The source's node ID: a Previous Node names a node (§4.4.1).
        emit(&"ipn:1.3.0".parse::<eid::Eid>().unwrap()).0
    } else if block_type == block::Type::BundleAge {
        emit(&5_000u64).0
    } else {
        emit(&HopInfo {
            limit: NonZeroU8::new(64).unwrap(),
            count: 1,
        })
        .0
    };
    let (_, data) = builder::Builder::new(source.clone(), "ipn:1.2.99".parse().unwrap())
        .add_extension_block(block_type)
        .expect("add the per-hop block")
        .build(body.into())
        .with_payload(b"per-hop".as_slice().into())
        .build(creation_timestamp::CreationTimestamp::now())
        .unwrap();
    let built = parse::parse(Bytes::from(data)).unwrap();
    let target = *built
        .bundle
        .blocks
        .iter()
        .find(|(_, b)| b.block_type == block_type)
        .unwrap()
        .0;
    let (sign_key, enc_key) = generated_keys();

    let signed = parse::parse(Bytes::from(
        signer::Signer::new(&built.bundle, &built.data)
            .sign_block(
                target,
                signer::Context::HMAC_SHA2(ScopeFlags::default()),
                source.clone(),
                &sign_key,
            )
            .map_err(|(_, e)| e)
            .expect("sign the per-hop block")
            .rebuild()
            .expect("rebuild signed"),
    ))
    .unwrap();
    let flags = ScopeFlags {
        include_security_header: false,
        ..ScopeFlags::default()
    };
    let mut encrypting = encryptor::Encryptor::new(&signed.bundle, &signed.data)
        .encrypt_block(
            target,
            encryptor::Context::AES_GCM(flags.clone()),
            source.clone(),
            &enc_key,
        )
        .map_err(|(_, e)| e)
        .expect("encrypt the per-hop block, and so its BIB");
    if encrypt_payload {
        encrypting = encrypting
            .encrypt_block(1, encryptor::Context::AES_GCM(flags), source, &enc_key)
            .map_err(|(_, e)| e)
            .expect("encrypt the payload");
    }
    let parsed = parse::parse(Bytes::from(
        encrypting.rebuild().expect("rebuild encrypted"),
    ))
    .unwrap();
    (parsed, target, enc_key)
}

// A bundle from ipn:1.3.1 with extension blocks of types 199 and 200,
// neither carrying a CRC, as RFC 9173 §3.8.1 and §4.8.1 leave a target, and
// a payload, whose source signed the blocks of `targets` into one BIB and
// then encrypted them, so the BIB is encrypted with them: the keyless parse,
// the targets' block numbers, and the signing and encryption keys.
fn signed_together_then_encrypted(
    targets: &[block::Type],
) -> (Parsed, Vec<u64>, bpsec::key::Key, bpsec::key::Key) {
    let (sign_key, enc_key) = generated_keys();
    let (parsed, numbers) = signed_together_then_encrypted_with(targets, &sign_key, &enc_key);
    (parsed, numbers, sign_key, enc_key)
}

// `signed_together_then_encrypted`, signed with `sign_key` and encrypted
// with `enc_key`: the keyless parse and the targets' block numbers.
fn signed_together_then_encrypted_with(
    targets: &[block::Type],
    sign_key: &bpsec::key::Key,
    enc_key: &bpsec::key::Key,
) -> (Parsed, Vec<u64>) {
    use bpsec::{encryptor, rfc9173::ScopeFlags, signer};

    let source: eid::Eid = "ipn:1.3.1".parse().unwrap();
    let (_, data) = builder::Builder::new(source.clone(), "ipn:1.2.99".parse().unwrap())
        .add_extension_block(block::Type::Unrecognised(199))
        .unwrap()
        .with_crc_type(crc::CrcType::None)
        .build(b"one".as_slice().into())
        .add_extension_block(block::Type::Unrecognised(200))
        .unwrap()
        .with_crc_type(crc::CrcType::None)
        .build(b"two".as_slice().into())
        .with_payload(b"payload".as_slice().into())
        .build(creation_timestamp::CreationTimestamp::now())
        .unwrap();
    let built = parse::parse(Bytes::from(data)).unwrap();
    let numbers: Vec<u64> = targets.iter().map(|&t| block_number(&built, t)).collect();
    // Without the security header in scope, the results share one BIB.
    let flags = ScopeFlags {
        include_security_header: false,
        ..ScopeFlags::default()
    };

    let mut signing = signer::Signer::new(&built.bundle, &built.data);
    for &n in &numbers {
        signing = signing
            .sign_block(
                n,
                signer::Context::HMAC_SHA2(flags.clone()),
                source.clone(),
                sign_key,
            )
            .map_err(|(_, e)| e)
            .expect("sign the target");
    }
    let signed = parse::parse(Bytes::from(signing.rebuild().expect("rebuild signed"))).unwrap();
    let mut encrypting = encryptor::Encryptor::new(&signed.bundle, &signed.data);
    for &n in &numbers {
        encrypting = encrypting
            .encrypt_block(
                n,
                encryptor::Context::AES_GCM(flags.clone()),
                source.clone(),
                enc_key,
            )
            .map_err(|(_, e)| e)
            .expect("encrypt the target");
    }
    let parsed = parse::parse(Bytes::from(
        encrypting.rebuild().expect("rebuild encrypted"),
    ))
    .unwrap();
    (parsed, numbers)
}

// The acceptance of `acceptors`, both services, over a fresh parse of
// `parsed` with `keys`: `Parsed::check` over them, one `Facts`, and the
// plaintext cache it filled.
fn accept_with(
    parsed: &Parsed,
    acceptors: &[u64],
    keys: Vec<bpsec::key::Key>,
) -> (Result<checks::Facts, Error>, bpsec::Decrypted) {
    let mut parsed = signed_then_encrypted_again(parsed);
    let keys = bpsec::key::KeySet::new(keys);
    let roles: HashMap<u64, Roles> = acceptors
        .iter()
        .map(|&n| (n, Roles::each(Role::Acceptor, SecurityServices::BOTH)))
        .collect();
    let facts = parsed.check(&keys, &roles, Roles::default());
    (facts, parsed.decrypted)
}

// The only BIB in `parsed`.
fn the_bib(parsed: &Parsed) -> u64 {
    block_number(parsed, block::Type::BlockIntegrity)
}

// Accepting a block's BCB operation takes with it the BCB operations on the
// encrypted BIB that covers it and on that BIB's other target, all or none
// (RFC 9172 §3.9): with both targets accepted, all three are decrypted, and
// the BIB's operation on each target is checked over its plaintext.
#[test]
fn accept_takes_the_bcb_operations_coupled_to_a_block() {
    let (parsed, targets, sign_key, enc_key) = signed_together_then_encrypted(&[
        block::Type::Unrecognised(199),
        block::Type::Unrecognised(200),
    ]);
    let (one, two, bib) = (targets[0], targets[1], the_bib(&parsed));

    let (facts, decrypted) = accept_with(&parsed, &[one, two], vec![sign_key, enc_key]);
    let facts = facts.expect("the acceptance completes");
    assert!(
        !facts.any(Checked::Failed) && !facts.any(Checked::NoKey) && facts.incomplete.is_empty(),
        "{facts:?}"
    );
    for n in [one, two, bib] {
        assert!(
            decrypted.plaintext.contains_key(&n),
            "block {n} is decrypted"
        );
    }
}

// A block whose BCB operation goes with an accepted block's and fails to
// decrypt leaves that block's acceptance incomplete: the failure is the
// corrupt block's own, and nothing coupled to it is accepted.
#[test]
fn accept_cannot_complete_a_block_coupled_to_a_corrupt_one() {
    let (parsed, targets, sign_key, enc_key) = signed_together_then_encrypted(&[
        block::Type::Unrecognised(199),
        block::Type::Unrecognised(200),
    ]);
    let (one, two) = (targets[0], targets[1]);
    let last = usize::try_from(parsed.bundle.blocks[&two].payload_range().end).unwrap() - 1;
    let mut data = parsed.data.to_vec();
    data[last] ^= 0x01;
    let corrupt = parse::parse(Bytes::from(data)).unwrap();

    let (facts, _) = accept_with(&corrupt, &[one, two], vec![sign_key, enc_key]);
    let facts = facts.expect("a failure is a fact");
    assert_eq!(
        found_targets(&facts, Checked::Failed),
        vec![two],
        "the corrupt block's operation fails"
    );
    assert_eq!(
        facts.incomplete.as_slice(),
        &[one],
        "its neighbour's acceptance cannot complete"
    );
}

// A block whose encrypted BIB also covers a block the caller does not
// accept, the payload or another, cannot be accepted: the BCB operation on
// that block goes with the accepted block's, all or none (RFC 9172 §3.9),
// and it is not the caller's to remove. Neither is decrypted.
#[test]
fn accept_cannot_complete_a_block_coupled_to_one_it_does_not_accept() {
    for partner in [block::Type::Payload, block::Type::Unrecognised(200)] {
        let (parsed, targets, sign_key, enc_key) =
            signed_together_then_encrypted(&[block::Type::Unrecognised(199), partner]);
        let (one, other) = (targets[0], targets[1]);

        let (facts, decrypted) = accept_with(&parsed, &[one], vec![sign_key, enc_key]);
        assert_eq!(
            facts
                .expect("an incomplete acceptance is a fact")
                .incomplete
                .as_slice(),
            &[one],
            "{partner:?}"
        );
        assert!(
            !decrypted.plaintext.contains_key(&one) && !decrypted.plaintext.contains_key(&other),
            "{partner:?}: neither the block nor its partner is decrypted"
        );
    }
}

// Without the BIB's key, its operations on the accepted blocks go
// unchecked; without the BCB's, the encrypted BIB cannot be read, and the
// blocks' coverage stays unknown, so their acceptance cannot complete.
#[test]
fn accept_reports_what_it_holds_no_key_for() {
    let (parsed, targets, sign_key, enc_key) = signed_together_then_encrypted(&[
        block::Type::Unrecognised(199),
        block::Type::Unrecognised(200),
    ]);
    let (one, two, bib) = (targets[0], targets[1], the_bib(&parsed));

    let (facts, _) = accept_with(&parsed, &[one, two], vec![enc_key]);
    let unchecked: HashSet<(u64, u64)> =
        found(&facts.expect("a missing key is a fact"), Checked::NoKey)
            .into_iter()
            .collect();
    assert_eq!(unchecked, HashSet::from([(bib, one), (bib, two)]));
    let (facts, _) = accept_with(&parsed, &[one, two], vec![sign_key]);
    let incomplete: HashSet<u64> = facts
        .expect("a missing key is a fact")
        .incomplete
        .iter()
        .copied()
        .collect();
    assert_eq!(incomplete, HashSet::from([one, two]));
}

// A BIB on the primary block is checked like any other accepted block's: a
// pass, a failure for another key, and a missing key are told apart.
#[test]
fn accept_checks_a_bib_on_the_primary_block() {
    use bpsec::{rfc9173::ScopeFlags, signer};

    let (signing, rotated) = distinct_keys(32);
    let built = per_hop_bundle(creation_timestamp::CreationTimestamp::now());
    let signed = parse::parse(Bytes::from(
        signer::Signer::new(&built.bundle, &built.data)
            .sign_block(
                0,
                signer::Context::HMAC_SHA2(ScopeFlags::default()),
                "ipn:1.3.1".parse().unwrap(),
                &hmac_key(&signing),
            )
            .map_err(|(_, e)| e)
            .expect("sign the primary block")
            .rebuild()
            .expect("rebuild the signed bundle"),
    ))
    .unwrap();
    let bib = the_bib(&signed);

    let (facts, _) = accept_with(&signed, &[0], vec![hmac_key(&signing)]);
    let facts = facts.expect("the check passes");
    assert!(!facts.any(Checked::Failed) && !facts.any(Checked::NoKey));
    let (facts, _) = accept_with(&signed, &[0], vec![hmac_key(&rotated)]);
    assert_eq!(
        found(&facts.expect("a failure is a fact"), Checked::Failed),
        vec![(bib, 0)]
    );
    let (facts, _) = accept_with(&signed, &[0], vec![]);
    assert_eq!(
        found(&facts.expect("a missing key is a fact"), Checked::NoKey),
        vec![(bib, 0)]
    );
}

// The payload's own operations are processed as it streams, so `accept`
// does not decrypt the payload; it decrypts the encrypted BIB over it, for
// the payload's acceptor to verify against the plaintext, and reports a key
// it lacks for either operation before the first byte.
#[test]
fn accept_settles_the_payloads_keys_without_decrypting_it() {
    let (parsed, _, sign_key, enc_key) = signed_together_then_encrypted(&[block::Type::Payload]);
    let bib = the_bib(&parsed);

    let (facts, decrypted) = accept_with(&parsed, &[1], vec![sign_key.clone(), enc_key.clone()]);
    let facts = facts.expect("nothing fails");
    assert!(facts.incomplete.is_empty() && !facts.any(Checked::Failed));
    assert!(!facts.any(Checked::NoKey));
    assert!(!decrypted.plaintext.contains_key(&1), "the payload streams");
    assert!(
        decrypted.plaintext.contains_key(&bib),
        "the encrypted BIB over the payload is decrypted"
    );

    let (facts, _) = accept_with(&parsed, &[1], vec![sign_key]);
    assert_eq!(found_targets(&facts.unwrap(), Checked::NoKey), vec![1]);

    let (facts, _) = accept_with(&parsed, &[1], vec![enc_key]);
    assert_eq!(found(&facts.unwrap(), Checked::NoKey), vec![(bib, 1)]);
}

// An operation in a security context this node does not support, on a block
// `accept` accepts, is reported rather than processed: on an extension block,
// and on the payload when it is accepted, though its operations are
// otherwise its acceptor's as it streams.
#[test]
fn accept_reports_an_unsupported_operation() {
    let extension = insert_after_primary(
        &build_minimal_bundle(),
        &[
            &make_block(199, 2, 0, b"covered"),
            &make_block(11, 3, 0, &make_unknown_context_asb(2)),
        ],
    );
    let parsed = parse::parse(Bytes::from(extension)).unwrap();
    let (facts, _) = accept_with(&parsed, &[2], vec![]);
    assert_eq!(
        found(
            &facts.expect("an unsupported operation is a fact"),
            Checked::Unsupported
        ),
        vec![(3, 2)]
    );

    let payload = insert_after_primary(
        &build_minimal_bundle(),
        &[&make_block(11, 2, 0, &make_unknown_context_asb(1))],
    );
    let parsed = parse::parse(Bytes::from(payload)).unwrap();
    let (facts, _) = accept_with(&parsed, &[1], vec![]);
    assert_eq!(
        found(
            &facts.expect("an unsupported operation is a fact"),
            Checked::Unsupported
        ),
        vec![(2, 1)]
    );
}

// Where the caller accepts the payload too, as a destination does, a block
// whose encrypted BIB also covers the payload is accepted like any other:
// it is decrypted, while the payload is left to stream, and the BIB's
// operation on it is checked over its plaintext, failing under another key.
#[test]
fn accept_checks_a_block_tied_to_an_accepted_payload() {
    let (signing, rotated) = distinct_keys(32);
    let enc_key = generated_keys().1;
    let (parsed, targets) = signed_together_then_encrypted_with(
        &[block::Type::Unrecognised(199), block::Type::Payload],
        &hmac_key(&signing),
        &enc_key,
    );
    let (one, bib) = (targets[0], the_bib(&parsed));

    let (facts, decrypted) = accept_with(
        &parsed,
        &[one, 1],
        vec![hmac_key(&signing), enc_key.clone()],
    );
    let facts = facts.expect("the acceptance completes");
    assert!(
        !facts.any(Checked::Failed) && !facts.any(Checked::NoKey) && facts.incomplete.is_empty(),
        "{facts:?}"
    );
    assert!(
        decrypted.plaintext.contains_key(&one),
        "the block is decrypted"
    );
    assert!(!decrypted.plaintext.contains_key(&1), "the payload streams");

    let (facts, _) = accept_with(&parsed, &[one, 1], vec![hmac_key(&rotated), enc_key]);
    assert_eq!(
        found(&facts.expect("a failure is a fact"), Checked::Failed),
        vec![(bib, one)]
    );
}

// An operation in a security context this node does not support, on a
// per-hop block, is reported to the caller, the next node being its
// acceptor, whatever the bundle's clock: whether the bundle can lose the
// block is the caller's, a fate to decide.
#[test]
fn verify_reports_an_unsupported_operation_on_a_per_hop_block() {
    let with_unsupported_bib = |timestamp| {
        let built = per_hop_bundle(timestamp);
        let age = block_number(&built, block::Type::BundleAge);
        let data = insert_after_primary(
            &built.data,
            &[&make_block(11, 9, 0, &make_unknown_context_asb(age))],
        );
        (parse::parse(Bytes::from(data)).unwrap(), age)
    };

    for timestamp in [
        creation_timestamp::CreationTimestamp::now(),
        creation_timestamp::CreationTimestamp::default(),
    ] {
        let (mut parsed, age) = with_unsupported_bib(timestamp);
        let facts = verify_with(&mut parsed, vec![]).expect("an unsupported operation is a fact");
        assert_eq!(found(&facts, Checked::Unsupported), vec![(9, age)]);
    }
}

// A signing key and an encryption key for ipn:1.3.1, the fixtures' security
// source. Immaterial key values: generated per the no-literal-keys rule.
fn generated_keys() -> (bpsec::key::Key, bpsec::key::Key) {
    let sign_key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:1.3.1",
        "kty": "oct",
        "alg": "HS256",
        "key_ops": ["sign", "verify"],
        "k": rand_k(32)
    }))
    .unwrap();
    let enc_key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:1.3.1",
        "kty": "oct",
        "alg": "A128KW",
        "enc": "A128GCM",
        "key_ops": ["encrypt", "decrypt", "wrapKey", "unwrapKey"],
        "k": rand_k(16)
    }))
    .unwrap();
    (sign_key, enc_key)
}

// A fresh parse of `parsed`'s bytes: a second pass starts from the bundle as
// received, not from what an earlier pass stamped.
fn signed_then_encrypted_again(parsed: &Parsed) -> Parsed {
    parse::parse(parsed.data.clone()).unwrap()
}

// The receiver's keyed pass over `parsed` with `keys`.
fn verify_with(parsed: &mut Parsed, keys: Vec<bpsec::key::Key>) -> Result<checks::Facts, Error> {
    common::verify_as_receiver(parsed, &bpsec::key::KeySet::new(keys), &HashSet::new())
}

// The encrypted BIB in `parsed`, by block number.
fn encrypted_bib(parsed: &Parsed) -> u64 {
    *parsed
        .bundle
        .blocks
        .iter()
        .find(|(_, b)| b.block_type == block::Type::BlockIntegrity && b.bcb.is_some())
        .expect("the bundle carries an encrypted BIB")
        .0
}

// An encrypted BIB over a Previous Node or Bundle Age block is a
// combination no valid bundle carries. With that block the only encrypted
// one besides the BIB, the structure alone proves it, and a keyless pass
// reports the BIB, unread, with the block it may cover.
#[test]
fn the_structure_proves_an_encrypted_bib_over_a_per_hop_block() {
    for block_type in [block::Type::PreviousNode, block::Type::BundleAge] {
        let (mut parsed, target, _) = signed_then_encrypted(block_type, false);
        let bib = encrypted_bib(&parsed);
        let facts = verify_with(&mut parsed, vec![])
            .unwrap_or_else(|e| panic!("{block_type:?} under an encrypted BIB is reported: {e}"));
        assert!(
            found(&facts, Checked::Conflicting).is_empty(),
            "{block_type:?}: nothing is proven by decryption"
        );
        assert_eq!(
            found(&facts, Checked::PossiblyConflicting),
            vec![(bib, target)],
            "{block_type:?}: the structure names the BIB and the block"
        );
    }
}

// With the payload encrypted too, a keyless node cannot tell which block the
// encrypted BIB covers, and reports nothing. Nor does a keyed pass that
// accepts the per-hop block alone: no BIB is read on a Previous Node's
// account. A pass that accepts the payload, under a BCB from the same
// security source, reads the BIB, resolves the coverage and proves it.
#[test]
fn a_keyed_pass_proves_an_encrypted_bib_over_a_per_hop_block() {
    let (mut parsed, target, enc_key) = signed_then_encrypted(block::Type::PreviousNode, true);
    let bib = encrypted_bib(&parsed);
    let facts =
        verify_with(&mut parsed, vec![]).expect("without the key, nothing proves the coverage");
    assert!(
        found(&facts, Checked::Conflicting).is_empty()
            && found(&facts, Checked::PossiblyConflicting).is_empty()
    );

    let mut parsed = signed_then_encrypted_again(&parsed);
    let facts = verify_with(&mut parsed, vec![enc_key.clone()])
        .expect("a per-hop acceptor reads no BIB on its account");
    assert!(
        found(&facts, Checked::Conflicting).is_empty()
            && found(&facts, Checked::PossiblyConflicting).is_empty()
    );
    assert!(
        !parsed.decrypted.plaintext.contains_key(&bib),
        "the BIB is not read on the Previous Node's account"
    );

    let mut parsed = signed_then_encrypted_again(&parsed);
    let facts = common::verify_as_receiver(
        &mut parsed,
        &bpsec::key::KeySet::new(vec![enc_key]),
        &HashSet::from([1]),
    )
    .expect("the payload's acceptor reads the BIB and reports the operation");
    assert_eq!(found(&facts, Checked::Conflicting), vec![(bib, target)]);
    assert!(found(&facts, Checked::PossiblyConflicting).is_empty());
}

// A keyed pass reads an encrypted BIB only on the account of an accepted
// block under a BCB from the same security source, since the node that
// encrypts a signed block encrypts its BIB too (RFC 9172 §3.9), and never on
// a Previous Node's or Bundle Age's. The source's encrypted BIB over its
// payload, which no per-hop acceptor has reason to read, stays unread, on
// fragment 0, where its BCB cannot authenticate against the fragment's
// primary block, as on the whole bundle; the previous hop's BIB over the Hop
// Count it encrypted is read, and the Hop Count verified under it.
#[test]
fn a_per_hop_acceptor_reads_only_its_sources_encrypted_bibs() {
    use bpsec::{encryptor, rfc9173::ScopeFlags, signer};
    use core::num::NonZeroU8;
    use hardy_bpv7::hop_info::HopInfo;
    use hardy_cbor::encode::emit;

    let source: eid::Eid = "ipn:1.3.1".parse().unwrap();
    let previous: eid::Eid = "ipn:1.3.0".parse().unwrap();
    let (sign_key, enc_key) = generated_keys();
    let (_, data) = builder::Builder::new(source.clone(), "ipn:1.2.99".parse().unwrap())
        .add_extension_block(block::Type::PreviousNode)
        .expect("add the Previous Node")
        .build(emit(&previous).0.into())
        .add_extension_block(block::Type::HopCount)
        .expect("add the Hop Count")
        .build(
            emit(&HopInfo {
                limit: NonZeroU8::new(64).unwrap(),
                count: 1,
            })
            .0
            .into(),
        )
        .with_payload(b"per-hop".as_slice().into())
        .build(creation_timestamp::CreationTimestamp::now())
        .unwrap();
    let built = parse::parse(Bytes::from(data)).unwrap();
    let previous_node = block_number(&built, block::Type::PreviousNode);
    let hop_count = block_number(&built, block::Type::HopCount);

    // The source signs its payload and encrypts it, its BIB with it, the
    // primary block in both scopes.
    let signed = parse::parse(Bytes::from(
        signer::Signer::new(&built.bundle, &built.data)
            .sign_block(
                1,
                signer::Context::HMAC_SHA2(ScopeFlags::default()),
                source.clone(),
                &sign_key,
            )
            .map_err(|(_, e)| e)
            .expect("sign the payload")
            .rebuild()
            .expect("rebuild signed"),
    ))
    .unwrap();
    let sources_bib = the_bib(&signed);
    let protected = parse::parse(Bytes::from(
        encryptor::Encryptor::new(&signed.bundle, &signed.data)
            .encrypt_block(
                1,
                encryptor::Context::AES_GCM(ScopeFlags::default()),
                source,
                &enc_key,
            )
            .map_err(|(_, e)| e)
            .expect("encrypt the payload")
            .rebuild()
            .expect("rebuild encrypted"),
    ))
    .unwrap();

    // The previous hop signs the Hop Count and encrypts it, its BIB with
    // it, and encrypts the Previous Node, each under a scope that leaves
    // the primary block out, so its operations survive fragmentation.
    let scope = ScopeFlags {
        include_primary_block: false,
        ..ScopeFlags::default()
    };
    let signed = parse::parse(Bytes::from(
        signer::Signer::new(&protected.bundle, &protected.data)
            .sign_block(
                hop_count,
                signer::Context::HMAC_SHA2(scope.clone()),
                previous.clone(),
                &sign_key,
            )
            .map_err(|(_, e)| e)
            .expect("sign the Hop Count")
            .rebuild()
            .expect("rebuild signed"),
    ))
    .unwrap();
    let whole = parse::parse(Bytes::from(
        encryptor::Encryptor::new(&signed.bundle, &signed.data)
            .encrypt_block(
                hop_count,
                encryptor::Context::AES_GCM(scope.clone()),
                previous.clone(),
                &enc_key,
            )
            .map_err(|(_, e)| e)
            .expect("encrypt the Hop Count, and so its BIB")
            .encrypt_block(
                previous_node,
                encryptor::Context::AES_GCM(scope),
                previous,
                &enc_key,
            )
            .map_err(|(_, e)| e)
            .expect("encrypt the Previous Node")
            .rebuild()
            .expect("rebuild encrypted"),
    ))
    .unwrap();
    let hop_counts_bib = *whole
        .bundle
        .blocks
        .iter()
        .find(|(n, b)| b.block_type == block::Type::BlockIntegrity && **n != sources_bib)
        .expect("the previous hop's BIB is present")
        .0;

    for (case, mut parsed) in [
        ("the whole bundle", signed_then_encrypted_again(&whole)),
        ("fragment 0", as_fragment(&whole)),
    ] {
        let facts = verify_with(&mut parsed, vec![sign_key.clone(), enc_key.clone()])
            .expect("the receiver's pass reports");
        assert!(
            facts.outcomes.iter().all(|o| o.target != sources_bib),
            "{case}: the source's BIB is read on no per-hop block's account: {:?}",
            facts.outcomes
        );
        assert!(
            !parsed.decrypted.plaintext.contains_key(&sources_bib)
                && parsed.decrypted.failed.is_empty(),
            "{case}: the source's BIB stays unread"
        );
        let mut expected = vec![previous_node, hop_count, hop_count, hop_counts_bib];
        expected.sort_unstable();
        assert_eq!(
            found_targets(&facts, Checked::Verified),
            expected,
            "{case}: {:?}",
            facts.outcomes
        );
        assert_eq!(facts.outcomes.len(), 4, "{case}: {:?}", facts.outcomes);
        assert!(facts.incomplete.is_empty(), "{case}");
    }
}

// The Hop Count is exempt: its increment is a SHOULD, so a signature on a
// count no node has incremented can stand inside an encrypted BIB.
#[test]
fn a_hop_count_under_an_encrypted_bib_passes() {
    let (mut parsed, _, enc_key) = signed_then_encrypted(block::Type::HopCount, false);
    verify_with(&mut parsed, vec![]).expect("a keyless node passes it");
    let mut parsed = signed_then_encrypted_again(&parsed);
    verify_with(&mut parsed, vec![enc_key]).expect("a key-holding node passes it");
}

// `parsed` as fragment 0 of a bundle of 1 MiB, every block as it is: a fresh
// parse.
fn as_fragment(parsed: &Parsed) -> Parsed {
    let chunks = Editor::new(&parsed.bundle, &parsed.data)
        .with_fragment_info(Some(FragmentInfo {
            offset: 0,
            total_adu_length: 1 << 20,
        }))
        .map_err(|(_, e)| e)
        .expect("set the fragment info")
        .rebuild()
        .expect("rebuild the fragment");
    parse::parse(Chunk::flatten_bytes(chunks, parsed.data.clone())).unwrap()
}

// A bundle from ipn:1.3.1 with a per-hop block of `block_type`, which its
// source signed where `signed` and encrypted otherwise, under a scope that
// leaves the primary block out where `primary_excluded`, and a payload its
// source signed in plaintext, made a fragment: the fresh parse, the per-hop
// block's number and the keys.
fn fragment_with_protected_per_hop_block(
    block_type: block::Type,
    signed: bool,
    primary_excluded: bool,
) -> (Parsed, u64, Vec<bpsec::key::Key>) {
    use bpsec::{encryptor, rfc9173::ScopeFlags, signer};
    use core::num::NonZeroU8;
    use hardy_bpv7::hop_info::HopInfo;
    use hardy_cbor::encode::emit;

    let scope = ScopeFlags {
        include_primary_block: !primary_excluded,
        ..ScopeFlags::default()
    };

    let source: eid::Eid = "ipn:1.3.1".parse().unwrap();
    let body = if block_type == block::Type::PreviousNode {
        emit(&"ipn:1.3.0".parse::<eid::Eid>().unwrap()).0
    } else if block_type == block::Type::BundleAge {
        emit(&5_000u64).0
    } else {
        emit(&HopInfo {
            limit: NonZeroU8::new(64).unwrap(),
            count: 1,
        })
        .0
    };
    let (_, data) = builder::Builder::new(source.clone(), "ipn:1.2.99".parse().unwrap())
        .add_extension_block(block_type)
        .expect("add the per-hop block")
        .build(body.into())
        .with_payload(b"per-hop".as_slice().into())
        .build(creation_timestamp::CreationTimestamp::now())
        .unwrap();
    let built = parse::parse(Bytes::from(data)).unwrap();
    let target = block_number(&built, block_type);
    let (sign_key, enc_key) = generated_keys();
    let signed_payload = parse::parse(Bytes::from(
        signer::Signer::new(&built.bundle, &built.data)
            .sign_block(
                1,
                signer::Context::HMAC_SHA2(ScopeFlags::default()),
                source.clone(),
                &sign_key,
            )
            .map_err(|(_, e)| e)
            .expect("sign the payload")
            .rebuild()
            .expect("rebuild signed"),
    ))
    .unwrap();
    let protected = if signed {
        signer::Signer::new(&signed_payload.bundle, &signed_payload.data)
            .sign_block(target, signer::Context::HMAC_SHA2(scope), source, &sign_key)
            .map_err(|(_, e)| e)
            .expect("sign the per-hop block")
            .rebuild()
            .expect("rebuild signed")
    } else {
        encryptor::Encryptor::new(&signed_payload.bundle, &signed_payload.data)
            .encrypt_block(target, encryptor::Context::AES_GCM(scope), source, &enc_key)
            .map_err(|(_, e)| e)
            .expect("encrypt the per-hop block")
            .rebuild()
            .expect("rebuild encrypted")
    };
    let protected = parse::parse(Bytes::from(protected)).unwrap();
    (as_fragment(&protected), target, vec![sign_key, enc_key])
}

// On a fragment the per-hop blocks are processed as on any bundle, being
// the forwarder's writes to the fragment (RFC 9171 §5.4), and nothing else
// is checkable before reassembly (RFC 9172 §5.2): the operation over the
// per-hop block is checked, verified where it was computed over the
// fragment, as one whose scope leaves the primary block out is, and failed
// where it covers the primary block as it was before fragmentation; the
// one over the payload is deferred.
#[test]
fn a_fragments_per_hop_blocks_are_checked_and_the_rest_deferred() {
    for (block_type, signed) in [
        (block::Type::PreviousNode, true),
        (block::Type::PreviousNode, false),
        (block::Type::BundleAge, true),
        (block::Type::BundleAge, false),
        (block::Type::HopCount, true),
        (block::Type::HopCount, false),
    ] {
        for (primary_excluded, checked) in [(true, Checked::Verified), (false, Checked::Failed)] {
            let case = format!("{block_type:?} signed {signed}, expecting {checked:?}");
            let (mut fragment, target, keys) =
                fragment_with_protected_per_hop_block(block_type, signed, primary_excluded);
            let roles: HashMap<u64, Roles> =
                [(target, Roles::each(Role::Acceptor, SecurityServices::BOTH))]
                    .into_iter()
                    .collect();
            let facts = fragment
                .check(
                    &bpsec::key::KeySet::new(keys),
                    &roles,
                    Roles::each(Role::Verifier, SecurityServices::INTEGRITY),
                )
                .expect("the check reports");
            assert_eq!(
                found_targets(&facts, checked),
                vec![target],
                "{case}: {:?}",
                facts.outcomes
            );
            assert_eq!(
                found_targets(&facts, Checked::Deferred),
                vec![1],
                "{case}: {:?}",
                facts.outcomes
            );
            assert_eq!(facts.outcomes.len(), 2, "{case}: {:?}", facts.outcomes);
        }
    }
}

// Nothing on a fragment is checkable before reassembly (RFC 9172 §5.2):
// every operation in this node's roles is deferred, and no key is looked
// up, so a keyless check reports no missing key and decrypts nothing.
#[test]
fn a_fragments_operations_are_deferred_without_a_key() {
    use bpsec::{encryptor, rfc9173::ScopeFlags, signer};

    // Block 199 signed in plaintext, and the payload encrypted.
    let source: eid::Eid = "ipn:1.3.1".parse().unwrap();
    let (_, data) = builder::Builder::new(source.clone(), "ipn:1.2.99".parse().unwrap())
        .add_extension_block(block::Type::Unrecognised(199))
        .unwrap()
        .build(b"one".as_slice().into())
        .with_payload(b"payload".as_slice().into())
        .build(creation_timestamp::CreationTimestamp::now())
        .unwrap();
    let built = parse::parse(Bytes::from(data)).unwrap();
    let signed_block = block_number(&built, block::Type::Unrecognised(199));
    let (sign_key, enc_key) = generated_keys();
    let signed = parse::parse(Bytes::from(
        signer::Signer::new(&built.bundle, &built.data)
            .sign_block(
                signed_block,
                signer::Context::HMAC_SHA2(ScopeFlags::default()),
                source.clone(),
                &sign_key,
            )
            .map_err(|(_, e)| e)
            .expect("sign the block")
            .rebuild()
            .expect("rebuild signed"),
    ))
    .unwrap();
    let protected = parse::parse(Bytes::from(
        encryptor::Encryptor::new(&signed.bundle, &signed.data)
            .encrypt_block(
                1,
                encryptor::Context::AES_GCM(ScopeFlags::default()),
                source,
                &enc_key,
            )
            .map_err(|(_, e)| e)
            .expect("encrypt the payload")
            .rebuild()
            .expect("rebuild encrypted"),
    ))
    .unwrap();
    let mut fragment = as_fragment(&protected);
    let bib = the_bib(&fragment);
    let bcb = block_number(&fragment, block::Type::BlockSecurity);

    let roles: HashMap<u64, Roles> = [(1, Roles::each(Role::Acceptor, SecurityServices::BOTH))]
        .into_iter()
        .collect();
    let facts = fragment
        .check(
            &empty_keys(),
            &roles,
            Roles::each(Role::Verifier, SecurityServices::INTEGRITY),
        )
        .expect("the check reports");
    let mut deferred = vec![(bcb, 1), (bib, signed_block)];
    deferred.sort_unstable();
    assert_eq!(found(&facts, Checked::Deferred), deferred);
    assert_eq!(facts.outcomes.len(), 2, "{:?}", facts.outcomes);
    assert!(facts.incomplete.is_empty());
    assert!(
        fragment.decrypted.plaintext.is_empty() && fragment.decrypted.failed.is_empty(),
        "nothing on a fragment is decrypted"
    );
}

// The RFC 9173 Appendix A.4 wire vector: one BCB (block 2) covers both the
// encrypted BIB (block 3) and the payload (block 1). The Bundle Age block
// (block 4) RFC 9171 §4.4.2 requires of its zero creation time is inserted
// before the payload; no security result covers it.
fn rfc9173_a4_vector() -> Vec<u8> {
    hex_literal::hex!(
        "9f88070000820282010282028202018202820201820018281a000f4240850b0300
         005846438ed6208eb1c1ffb94d952175167df0902902064a2983910c4fb2340790bf
         420a7d1921d5bf7c4721e02ab87a93ab1e0b75cf62e4948727c8b5dae46ed2af0543
         9b88029191850c0201005849820301020182028202018382014c5477656c76653132
         313231328202038204078281820150220ffc45c8a901999ecc60991dd78b29818201
         50d2c51cb2481792dae8b21d848cede99b85070400004319012c8501010000582390
         eab6457593379298a8724e16e61f837488e127212b59ac91f8a86287b7d07630a122
         ff"
    )
    .to_vec()
}

// The RFC 9173 Appendix A.4 conformance keys, pinned together with the wire
// capture: the vector can only be regenerated with these exact keys.
fn rfc9173_a4_keys() -> bpsec::key::KeySet {
    serde_json::from_value(serde_json::json!({
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
    .unwrap()
}

// Flips one byte of the A.4 vector's BIB ciphertext: the block-3 body is the
// 70-byte string right after its `58 46` bytes header.
fn corrupt_a4_bib(data: &mut [u8]) {
    let pos = data
        .windows(2)
        .position(|w| w == hex_literal::hex!("5846"))
        .expect("BIB body header present")
        + 2;
    data[pos] ^= 0x01;
}

// RFC 9172 §3.9 over the A.4 vector with its BIB corrupted, at a node that
// accepts nothing the BIB may carry: the bundle has no per-hop block, so
// `check` leaves the BIB encrypted and unchecked though it holds every key,
// and reports nothing. The corruption is the shared BCB's acceptor's to find
// (`multi_target_bcb_failure_drop_spares_the_shared_bcb`).
#[test]
fn a_corrupt_bib_under_a_shared_bcb_is_left_to_its_acceptor() {
    let mut data = rfc9173_a4_vector();
    corrupt_a4_bib(&mut data);
    let mut parsed = parse::parse(Bytes::from(data)).unwrap();

    let facts = common::verify_as_receiver(&mut parsed, &rfc9173_a4_keys(), &HashSet::new())
        .expect("the corrupt BIB is not this node's to check");
    assert!(
        !parsed.decrypted.plaintext.contains_key(&3),
        "the BIB is not decrypted"
    );
    assert!(!facts.any(Checked::Failed), "no failure is reported");
}

// A bundle from ipn:1.3.1 whose payload its source signed then encrypted,
// which encrypts the BIB over it too (RFC 9172 §3.9), with an encrypted
// Bundle Age beside it when `encrypted_age` is set. Returns the keyless
// parse and the keys for every operation.
fn encrypted_payload_bib(encrypted_age: bool) -> (Parsed, Vec<bpsec::key::Key>) {
    use bpsec::{encryptor, rfc9173::ScopeFlags, signer};
    use hardy_cbor::encode::emit;

    let source: eid::Eid = "ipn:1.3.1".parse().unwrap();
    let mut building = builder::Builder::new(source.clone(), "ipn:1.2.99".parse().unwrap());
    if encrypted_age {
        building = building
            .add_extension_block(block::Type::BundleAge)
            .expect("add the Bundle Age")
            .build(emit(&5_000u64).0.into());
    }
    let (_, data) = building
        .with_payload(b"payload".as_slice().into())
        .build(creation_timestamp::CreationTimestamp::now())
        .unwrap();
    let built = parse::parse(Bytes::from(data)).unwrap();
    let (sign_key, enc_key) = generated_keys();

    let signed = parse::parse(Bytes::from(
        signer::Signer::new(&built.bundle, &built.data)
            .sign_block(
                1,
                signer::Context::HMAC_SHA2(ScopeFlags::default()),
                source.clone(),
                &sign_key,
            )
            .map_err(|(_, e)| e)
            .expect("sign the payload")
            .rebuild()
            .expect("rebuild signed"),
    ))
    .unwrap();
    let flags = ScopeFlags {
        include_security_header: false,
        ..ScopeFlags::default()
    };
    let mut encrypting = encryptor::Encryptor::new(&signed.bundle, &signed.data)
        .encrypt_block(
            1,
            encryptor::Context::AES_GCM(flags.clone()),
            source.clone(),
            &enc_key,
        )
        .map_err(|(_, e)| e)
        .expect("encrypt the payload, and so its BIB");
    if encrypted_age {
        let (&age, _) = signed
            .bundle
            .blocks
            .iter()
            .find(|(_, b)| b.block_type == block::Type::BundleAge)
            .expect("the Bundle Age is present");
        encrypting = encrypting
            .encrypt_block(age, encryptor::Context::AES_GCM(flags), source, &enc_key)
            .map_err(|(_, e)| e)
            .expect("encrypt the Bundle Age");
    }
    let parsed = parse::parse(Bytes::from(
        encrypting.rebuild().expect("rebuild encrypted"),
    ))
    .unwrap();
    (parsed, vec![sign_key, enc_key])
}

// RFC 9172 §3.9: only the acceptor of the BCB over an encrypted BIB decrypts
// it. Holding every key, `check` leaves the BIB over an encrypted payload
// encrypted when nothing it accepts under the same security source's BCB is
// beside it: an encrypted Bundle Age, whose operations this node accepts,
// is no account to read it on, since no valid bundle has an encrypted BIB
// over a block every forwarder replaces. The payload's acceptor, under that
// source's BCB, reads it.
#[test]
fn an_encrypted_bib_is_decrypted_only_by_necessity() {
    for encrypted_age in [false, true] {
        let (mut parsed, keys) = encrypted_payload_bib(encrypted_age);
        let (&bib, _) = parsed
            .bundle
            .blocks
            .iter()
            .find(|(_, b)| b.block_type == block::Type::BlockIntegrity)
            .expect("the payload's BIB is present");
        let keys = bpsec::key::KeySet::new(keys);
        common::verify_as_receiver(&mut parsed, &keys, &HashSet::new())
            .unwrap_or_else(|e| panic!("encrypted_age {encrypted_age}: the bundle verifies: {e}"));
        assert!(
            !parsed.decrypted.plaintext.contains_key(&bib),
            "encrypted_age {encrypted_age}: the BIB is read on no per-hop block's account"
        );
        assert_eq!(
            parsed.bundle.blocks[&1].bib,
            block::BibCoverage::Maybe,
            "encrypted_age {encrypted_age}: the payload's coverage stays unresolved"
        );

        let mut parsed = signed_then_encrypted_again(&parsed);
        common::verify_as_receiver(&mut parsed, &keys, &HashSet::from([1])).unwrap_or_else(|e| {
            panic!("encrypted_age {encrypted_age}: the payload's acceptor verifies: {e}")
        });
        assert!(
            parsed.decrypted.plaintext.contains_key(&bib),
            "encrypted_age {encrypted_age}: the payload's acceptor reads the BIB"
        );
        assert_eq!(
            parsed.bundle.blocks[&1].bib,
            block::BibCoverage::Some(bib),
            "encrypted_age {encrypted_age}: the coverage resolves once the BIB is read"
        );
    }
}

// A bundle from ipn:1.3.1 created at `timestamp`, carrying a Bundle Age
// block (one without a creation time must) and a Hop Count block.
fn per_hop_bundle(timestamp: creation_timestamp::CreationTimestamp) -> Parsed {
    use core::num::NonZeroU8;
    use hardy_bpv7::hop_info::HopInfo;
    use hardy_cbor::encode::emit;

    let (_, data) =
        builder::Builder::new("ipn:1.3.1".parse().unwrap(), "ipn:1.2.99".parse().unwrap())
            .add_extension_block(block::Type::BundleAge)
            .unwrap()
            .build(emit(&5_000u64).0.into())
            .with_hop_count(&HopInfo {
                limit: NonZeroU8::new(64).unwrap(),
                count: 1,
            })
            .with_payload(b"vital".as_slice().into())
            .build(timestamp)
            .unwrap();
    parse::parse(Bytes::from(data)).unwrap()
}

// The number of the block of `block_type` in `parsed`.
fn block_number(parsed: &Parsed, block_type: block::Type) -> u64 {
    let (&n, _) = parsed
        .bundle
        .blocks
        .iter()
        .find(|(_, b)| b.block_type == block_type)
        .expect("the block is present");
    n
}

// Two distinct generated key values, for a key and the one that replaced it.
fn distinct_keys(len: usize) -> (String, String) {
    let (a, b) = (rand_k(len), rand_k(len));
    assert_ne!(a, b, "the two keys differ");
    (a, b)
}

fn hmac_key(k: &str) -> bpsec::key::Key {
    serde_json::from_value(serde_json::json!({
        "kid": "ipn:1.3.1",
        "kty": "oct",
        "alg": "HS256",
        "key_ops": ["sign", "verify"],
        "k": k
    }))
    .unwrap()
}

fn aes_key(k: &str) -> bpsec::key::Key {
    serde_json::from_value(serde_json::json!({
        "kid": "ipn:1.3.1",
        "kty": "oct",
        "enc": "A128GCM",
        "key_ops": ["encrypt", "decrypt"],
        "k": k
    }))
    .unwrap()
}

// The receiver's keyed pass with `keys` over a fresh parse of `parsed`'s
// bytes, dropped with what it decrypts.
fn verify_keyed(parsed: &Parsed, keys: Vec<bpsec::key::Key>) -> Result<checks::Facts, Error> {
    let mut parsed = parse::parse(parsed.data.clone()).unwrap();
    verify_with(&mut parsed, keys)
}

// A failed integrity check is a fact for the caller's failure policy on
// every target alike, the primary block and the payload included: whether
// the bundle can lose the target (RFC 9171 §4.1) is the caller's, a fate to decide.
#[test]
fn a_failed_check_on_any_target_is_a_fact() {
    use bpsec::{rfc9173::ScopeFlags, signer};

    let (signing, rotated) = distinct_keys(32);
    let built = per_hop_bundle(creation_timestamp::CreationTimestamp::now());
    let hop_count = block_number(&built, block::Type::HopCount);
    let signed = |target: u64| {
        parse::parse(Bytes::from(
            signer::Signer::new(&built.bundle, &built.data)
                .sign_block(
                    target,
                    signer::Context::HMAC_SHA2(ScopeFlags::default()),
                    "ipn:1.3.1".parse().unwrap(),
                    &hmac_key(&signing),
                )
                .map_err(|(_, e)| e)
                .expect("sign the target")
                .rebuild()
                .expect("rebuild the signed bundle"),
        ))
        .unwrap()
    };

    for target in [0, 1, hop_count] {
        let facts = verify_keyed(&signed(target), vec![hmac_key(&rotated)])
            .expect("a failed check on block {target} is a fact");
        assert_eq!(found_targets(&facts, Checked::Failed), vec![target]);
    }
}

// A Bundle Age this node cannot decrypt, or holds no key for, is a fact
// whatever the bundle's clock: that a bundle without a creation time cannot
// lose the block, its only expiry signal (RFC 9171 §4.4.2), is the caller's,
// a fate to decide.
#[test]
fn the_bundle_ages_confidentiality_outcome_is_a_fact_whatever_the_clock() {
    use bpsec::{encryptor, rfc9173::ScopeFlags};

    let (encrypting, rotated) = distinct_keys(16);
    let encrypted_age = |timestamp| {
        let built = per_hop_bundle(timestamp);
        let age = block_number(&built, block::Type::BundleAge);
        let parsed = parse::parse(Bytes::from(
            encryptor::Encryptor::new(&built.bundle, &built.data)
                .encrypt_block(
                    age,
                    encryptor::Context::AES_GCM(ScopeFlags::default()),
                    "ipn:1.3.1".parse().unwrap(),
                    &aes_key(&encrypting),
                )
                .map_err(|(_, e)| e)
                .expect("encrypt the Bundle Age")
                .rebuild()
                .expect("rebuild the encrypted bundle"),
        ))
        .unwrap();
        (parsed, age)
    };

    for timestamp in [
        creation_timestamp::CreationTimestamp::default(),
        creation_timestamp::CreationTimestamp::now(),
    ] {
        let (parsed, age) = encrypted_age(timestamp);
        let facts = verify_keyed(&parsed, vec![]).expect("a missing key is a fact");
        assert_eq!(found_targets(&facts, Checked::NoKey), vec![age]);
        let facts =
            verify_keyed(&parsed, vec![aes_key(&rotated)]).expect("a failed decrypt is a fact");
        assert_eq!(found_targets(&facts, Checked::Failed), vec![age]);
    }
}

// The same holds for an integrity operation on that Bundle Age: a key this
// node does not hold is a fact, whatever the clock.
#[test]
fn the_bundle_ages_integrity_outcome_is_a_fact_whatever_the_clock() {
    use bpsec::{rfc9173::ScopeFlags, signer};

    let signing = rand_k(32);
    let signed_age = |timestamp| {
        let built = per_hop_bundle(timestamp);
        let age = block_number(&built, block::Type::BundleAge);
        let parsed = parse::parse(Bytes::from(
            signer::Signer::new(&built.bundle, &built.data)
                .sign_block(
                    age,
                    signer::Context::HMAC_SHA2(ScopeFlags::default()),
                    "ipn:1.3.1".parse().unwrap(),
                    &hmac_key(&signing),
                )
                .map_err(|(_, e)| e)
                .expect("sign the Bundle Age")
                .rebuild()
                .expect("rebuild the signed bundle"),
        ))
        .unwrap();
        (parsed, age)
    };

    for timestamp in [
        creation_timestamp::CreationTimestamp::default(),
        creation_timestamp::CreationTimestamp::now(),
    ] {
        let (parsed, age) = signed_age(timestamp);
        let facts = verify_keyed(&parsed, vec![]).expect("a missing key is a fact");
        assert_eq!(found_targets(&facts, Checked::NoKey), vec![age]);
    }
}
