use hardy_bpv7::{
    Bundle,
    bpsec::{self, edit::BPSecEditor, encryptor, key, rfc9173::ScopeFlags, signer},
    builder::Builder,
    checks,
    creation_timestamp::CreationTimestamp,
    editor::{Chunk, Editor},
    parse,
};

// Helper function to count blocks of a specific type
fn count_blocks_of_type(bundle: &Bundle, block_type: hardy_bpv7::block::Type) -> usize {
    bundle
        .blocks
        .values()
        .filter(|b| b.block_type == block_type)
        .count()
}

// Signer/Encryptor take a `&Bundle`; re-parse the bytes structurally to
// get one. The bundle and bytes are produced together by Builder, so this
// is a pure type-shape conversion.
fn raw_of(bytes: &[u8]) -> Bundle {
    parse::parse(::bytes::Bytes::copy_from_slice(bytes))
        .expect("parse")
        .bundle
}

// === Local keyed-validation helpers =================================
//
// An explicit composition of the per-section bpv7 helpers — same shape as
// bpv7-tools' `cmd::parse_with_keys` / `block_data` / `verify_block`, kept
// local to this test.

/// Structural parse + keyed BPSec validation (Sections A, B, C7).
/// Returns the parser-owned `Bytes` plus the bundle and decoded
/// BPSec OperationSets. NoKey is soft inside Section B / C7.
#[allow(clippy::type_complexity)]
fn validate_with_keys(
    data: &[u8],
    keys: &key::KeySet,
) -> Result<
    (
        ::bytes::Bytes,
        Bundle,
        std::collections::HashMap<u64, bpsec::bcb::OperationSet>,
        std::collections::HashMap<u64, bpsec::bib::OperationSet>,
    ),
    hardy_bpv7::Error,
> {
    let parse::Parsed {
        data,
        mut bundle,
        bcbs: bcb_ops,
        bibs: mut bib_ops,
    } = parse::parse(::bytes::Bytes::copy_from_slice(data))?;

    // §A — classify (Unsupported errors propagate)
    checks::classify_unsupported(&bundle.blocks, &bcb_ops, &bib_ops, &[])?;

    // §B — decrypt + validate BCB-covered BIBs (NoKey is soft)
    let mut decrypted = std::collections::HashMap::new();
    let no_updates = std::collections::HashMap::new();
    let all = checks::decrypt_and_validate_covered_bibs(
        &data,
        keys,
        &mut bundle.blocks,
        &bcb_ops,
        &mut bib_ops,
        &mut decrypted,
        &no_updates,
    )?;
    if all {
        checks::resolve_bib_coverage_maybes(&mut bundle.blocks);
    }

    // §C7 — verify every BIB with the supplied keys (NoKey is soft)
    checks::verify_all_bibs(
        &data,
        keys,
        &bundle.blocks,
        &bib_ops,
        &decrypted,
        &no_updates,
    )?;

    Ok((data, bundle, bcb_ops, bib_ops))
}

/// A `BlockSet` over `(blocks, bytes)` that decrypts on demand: if a
/// queried block is BCB-protected, it's decrypted on the fly so BIB
/// verification over BCB-encrypted targets sees the plaintext the BIB
/// actually signed (RFC 9172 §3.10 — sign before encrypt). Distinct from
/// the canonical [`bpsec::PlainBlockSet`], which returns raw wire bytes;
/// this recursion is why the per-block `verify_block` helper below can't
/// just use the plain one.
struct DecryptingBlockSet<'a> {
    blocks: &'a std::collections::HashMap<u64, hardy_bpv7::block::Block>,
    source_data: &'a [u8],
    bcb_ops: &'a std::collections::HashMap<u64, bpsec::bcb::OperationSet>,
    keys: &'a key::KeySet,
    /// Block number currently being verified — skip BCB-decryption for
    /// it to avoid infinite recursion when the target is itself the
    /// BCB-protected block we're trying to verify.
    skip_decrypt: Option<u64>,
}

impl<'a> bpsec::BlockSet<'a> for DecryptingBlockSet<'a> {
    fn block(
        &'a self,
        block_number: u64,
    ) -> Option<(
        &'a hardy_bpv7::block::Block,
        Option<hardy_bpv7::block::Payload<'a>>,
    )> {
        let block = self.blocks.get(&block_number)?;
        let payload = if let Some(bcb_num) = block.bcb {
            if Some(block_number) == self.skip_decrypt {
                // Caller (e.g. block_data for a BCB target) wants the
                // raw ciphertext bytes — don't recurse into decrypt.
                block
                    .payload(self.source_data)
                    .map(hardy_bpv7::block::Payload::Borrowed)
            } else {
                let opset = self.bcb_ops.get(&bcb_num)?;
                let op = opset.operations().get(&block_number)?;
                op.decrypt(
                    self.keys,
                    bpsec::bcb::OperationArgs {
                        bpsec_source: opset.source(),
                        target: block_number,
                        source: bcb_num,
                        blocks: &DecryptingBlockSet {
                            blocks: self.blocks,
                            source_data: self.source_data,
                            bcb_ops: self.bcb_ops,
                            keys: self.keys,
                            // Prevent recursion through the same BCB.
                            skip_decrypt: Some(block_number),
                        },
                    },
                )
                .ok()
                .map(hardy_bpv7::block::Payload::Decrypted)
            }
        } else {
            block
                .payload(self.source_data)
                .map(hardy_bpv7::block::Payload::Borrowed)
        };
        Some((block, payload))
    }
}

/// Per-block BIB verify. Returns `Ok(true)` when the block was
/// BIB-covered and verified, `Ok(false)` when it had no BIB, and
/// `Err(_)` for any verify failure (including `NoKey`). Handles
/// BCB-encrypted targets transparently via `DecryptingBlockSet`'s
/// on-demand decryption — RFC 9172 §3.10 sign-before-encrypt.
fn verify_block(
    block_number: u64,
    blocks: &std::collections::HashMap<u64, hardy_bpv7::block::Block>,
    data: &[u8],
    bcb_ops: &std::collections::HashMap<u64, bpsec::bcb::OperationSet>,
    bib_ops: &std::collections::HashMap<u64, bpsec::bib::OperationSet>,
    keys: &key::KeySet,
) -> Result<bool, hardy_bpv7::Error> {
    let target = blocks
        .get(&block_number)
        .ok_or(hardy_bpv7::Error::MissingBlock(block_number))?;
    let bib_block_number = match target.bib {
        hardy_bpv7::block::BibCoverage::Some(n) => n,
        hardy_bpv7::block::BibCoverage::None => return Ok(false),
        hardy_bpv7::block::BibCoverage::Maybe => {
            return Err(hardy_bpv7::Error::InvalidBPSec(bpsec::Error::MaybeHasBib(
                block_number,
            )));
        }
    };
    let opset = bib_ops
        .get(&bib_block_number)
        .ok_or(hardy_bpv7::Error::Altered)?;
    let op = opset
        .operations()
        .get(&block_number)
        .ok_or(hardy_bpv7::Error::Altered)?;
    let block_set = DecryptingBlockSet {
        blocks,
        source_data: data,
        bcb_ops,
        keys,
        skip_decrypt: None,
    };
    op.verify(
        keys,
        bpsec::bib::OperationArgs {
            bpsec_source: opset.source(),
            target: block_number,
            source: bib_block_number,
            blocks: &block_set,
        },
    )
    .map(|_| true)
    .map_err(hardy_bpv7::Error::InvalidBPSec)
}

/// Per-block plaintext: slice when unencrypted, BCB-decrypt when not.
fn block_data<'a>(
    block_number: u64,
    blocks: &'a std::collections::HashMap<u64, hardy_bpv7::block::Block>,
    data: &'a [u8],
    bcb_ops: &std::collections::HashMap<u64, bpsec::bcb::OperationSet>,
    keys: &key::KeySet,
) -> Result<hardy_bpv7::block::Payload<'a>, hardy_bpv7::Error> {
    let target = blocks
        .get(&block_number)
        .ok_or(hardy_bpv7::Error::MissingBlock(block_number))?;
    if let Some(bcb_num) = target.bcb {
        let opset = bcb_ops.get(&bcb_num).ok_or(hardy_bpv7::Error::Altered)?;
        let op = opset
            .operations()
            .get(&block_number)
            .ok_or(hardy_bpv7::Error::Altered)?;
        let block_set = DecryptingBlockSet {
            blocks,
            source_data: data,
            bcb_ops,
            keys,
            // We're already decrypting `block_number`; DecryptingBlockSet
            // must not recurse into another decrypt of the same target.
            skip_decrypt: Some(block_number),
        };
        op.decrypt(
            keys,
            bpsec::bcb::OperationArgs {
                bpsec_source: opset.source(),
                target: block_number,
                source: bcb_num,
                blocks: &block_set,
            },
        )
        .map(hardy_bpv7::block::Payload::Decrypted)
        .map_err(hardy_bpv7::Error::InvalidBPSec)
    } else {
        target
            .payload(data)
            .map(hardy_bpv7::block::Payload::Borrowed)
            .ok_or(hardy_bpv7::Error::Altered)
    }
}

#[test]
fn rfc9173_appendix_a_1() {
    // Original RFC9173 Appendix A.1.4 test vector
    // Note: No CRC on primary block, no Bundle Age - these checks are now in BPA filter
    let data = hex_literal::hex!(
        "9f88070000820282010282028202018202820201820018281a000f4240850b0200
                005856810101018202820201828201078203008181820158403bdc69b3a34a2b5d3a
                8554368bd1e808f606219d2a10a846eae3886ae4ecc83c4ee550fdfb1cc636b904e2
                f1a73e303dcd4b6ccece003e95e8164dcc89a156e185010100005823526561647920
                746f2067656e657261746520612033322d62797465207061796c6f6164ff"
    );
    let keys: key::KeySet = serde_json::from_value(serde_json::json!({
        "keys": [{
            "kid": "ipn:2.1",
            "kty": "oct",
            "alg": "HS512",
            "key_ops": ["verify"],
            "k": "GisaKxorGisaKxorGisaKw"
        }]
    }))
    .unwrap();

    let (data, raw, bcb_ops, bib_ops) = validate_with_keys(&data, &keys).unwrap();
    verify_block(1, &raw.blocks, &data, &bcb_ops, &bib_ops, &keys).expect("Failed to verify");
}

#[test]
fn rfc9173_appendix_a_2() {
    // Original RFC9173 Appendix A.2.4 test vector
    // Note: No CRC on primary block, no Bundle Age - these checks are now in BPA filter
    let data = hex_literal::hex!(
        "9f88070000820282010282028202018202820201820018281a000f4240850c0201
                0058508101020182028202018482014c5477656c7665313231323132820201820358
                1869c411276fecddc4780df42c8a2af89296fabf34d7fae7008204008181820150ef
                a4b5ac0108e3816c5606479801bc04850101000058233a09c1e63fe23a7f66a59c73
                03837241e070b02619fc59c5214a22f08cd70795e73e9aff"
    );
    let keys: key::KeySet = serde_json::from_value(serde_json::json!({
        "keys": [{
            "kid": "ipn:2.1",
            "kty": "oct",
            "alg": "A128KW",
            "enc": "A128GCM",
            "key_ops": ["unwrapKey", "decrypt"],
            "k": "YWJjZGVmZ2hpamtsbW5vcA"
        }]
    }))
    .unwrap();

    let (data, raw, bcb_ops, _bib_ops) = validate_with_keys(&data, &keys).unwrap();
    block_data(1, &raw.blocks, &data, &bcb_ops, &keys).expect("Failed to decrypt");
}

#[test]
fn rfc9173_appendix_a_3() {
    let data = hex_literal::hex!(
        "9f88070000820282010282028202018202820201820018281a000f4240850b0300
                00585c8200020101820282030082820105820300828182015820cac6ce8e4c5dae57
                988b757e49a6dd1431dc04763541b2845098265bc817241b81820158203ed614c0d9
                7f49b3633627779aa18a338d212bf3c92b97759d9739cd50725596850c0401005834
                8101020182028202018382014c5477656c7665313231323132820201820400818182
                0150efa4b5ac0108e3816c5606479801bc0485070200004319012c85010100005823
                3a09c1e63fe23a7f66a59c7303837241e070b02619fc59c5214a22f08cd70795e73e
                9aff"
    );
    let keys: key::KeySet = serde_json::from_value(serde_json::json!({
        "keys": [
            {
                "kid": "ipn:3.0",
                "kty": "oct",
                "alg": "HS256",
                "key_ops": ["verify"],
                "k": "GisaKxorGisaKxorGisaKw"
            },
            {
                "kid": "ipn:2.1",
                "kty": "oct",
                "alg": "dir",
                "enc": "A128GCM",
                "key_ops": ["decrypt"],
                "k": "cXdlcnR5dWlvcGFzZGZnaA"
            }
        ]
    }))
    .unwrap();

    let (data, raw, bcb_ops, bib_ops) = validate_with_keys(&data, &keys).unwrap();
    verify_block(2, &raw.blocks, &data, &bcb_ops, &bib_ops, &keys).expect("Failed to verify");
    verify_block(0, &raw.blocks, &data, &bcb_ops, &bib_ops, &keys).expect("Failed to verify");
    block_data(1, &raw.blocks, &data, &bcb_ops, &keys).expect("Failed to decrypt");
}

#[test]
fn rfc9173_appendix_a_4() {
    // Original RFC9173 Appendix A.4.5 test vector
    // Note: No CRC on primary block, no Bundle Age - these checks are now in BPA filter
    let data = hex_literal::hex!(
        "9f88070000820282010282028202018202820201820018281a000f4240850b0300
                005846438ed6208eb1c1ffb94d952175167df0902902064a2983910c4fb2340790bf
                420a7d1921d5bf7c4721e02ab87a93ab1e0b75cf62e4948727c8b5dae46ed2af0543
                9b88029191850c0201005849820301020182028202018382014c5477656c76653132
                313231328202038204078281820150220ffc45c8a901999ecc60991dd78b29818201
                50d2c51cb2481792dae8b21d848cede99b8501010000582390eab6457593379298a8
                724e16e61f837488e127212b59ac91f8a86287b7d07630a122ff"
    );
    let keys: key::KeySet = serde_json::from_value(serde_json::json!({
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

    let (data, raw, bcb_ops, bib_ops) = validate_with_keys(&data, &keys).unwrap();
    block_data(1, &raw.blocks, &data, &bcb_ops, &keys).expect("Failed to decrypt");
    verify_block(1, &raw.blocks, &data, &bcb_ops, &bib_ops, &keys).expect("Failed to verify");
}

// LLR 2.2.4, 2.2.7: Wrapped Key Unwrap
#[test]
fn test_wrapped_key_sign_and_verify() {
    // Use A128KW key-wrapping with HS256 HMAC — the sign operation generates
    // a random CEK, wraps it with the KEK, and includes the wrapped CEK in
    // the BIB parameters. Verification unwraps the CEK and uses it to verify.

    let (_bundle, bundle_bytes) =
        Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_payload(b"key-wrap test".as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();

    // Key with A128KW wrapping + HS256 HMAC
    let kek: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "HS256+A128KW",
        "key_ops": ["sign", "verify", "wrapKey", "unwrapKey"],
        "k": "AAAAAAAAAAAAAAAAAAAAAA"
    }))
    .unwrap();
    let keys = key::KeySet::new(vec![kek.clone()]);

    // Sign with key wrapping
    let raw = raw_of(&bundle_bytes);
    let signer = signer::Signer::new(&raw, &bundle_bytes)
        .sign_block(
            1,
            signer::Context::HMAC_SHA2(ScopeFlags::default()),
            "ipn:2.1".parse().unwrap(),
            &kek,
        )
        .map_err(|(_, e)| e)
        .expect("Failed to sign with key wrapping");
    let signed_bytes = signer.rebuild().expect("Failed to rebuild");

    // Verify — this unwraps the CEK from the BIB parameters
    let (signed_bytes, parsed, bcb_ops, bib_ops) =
        validate_with_keys(&signed_bytes, &keys).expect("Failed to parse signed bundle");
    verify_block(1, &parsed.blocks, &signed_bytes, &bcb_ops, &bib_ops, &keys)
        .expect("Key-wrap verification should succeed");
}

// LLR 2.2.4, 2.2.7: Wrapped Key Unwrap Failure
#[test]
fn test_wrapped_key_wrong_kek() {
    // Sign with one KEK, attempt to verify with a different KEK — unwrap should fail

    let (_bundle, bundle_bytes) =
        Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_payload(b"key-wrap fail test".as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();

    let sign_kek: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "HS256+A128KW",
        "key_ops": ["sign", "wrapKey"],
        "k": "AAAAAAAAAAAAAAAAAAAAAA"
    }))
    .unwrap();

    // Sign with the correct KEK
    let raw = raw_of(&bundle_bytes);
    let signer = signer::Signer::new(&raw, &bundle_bytes)
        .sign_block(
            1,
            signer::Context::HMAC_SHA2(ScopeFlags::default()),
            "ipn:2.1".parse().unwrap(),
            &sign_kek,
        )
        .map_err(|(_, e)| e)
        .expect("Failed to sign");
    let signed_bytes = signer.rebuild().expect("Failed to rebuild");

    // Verify with a DIFFERENT KEK — unwrap should fail
    let wrong_kek: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "HS256+A128KW",
        "key_ops": ["verify", "unwrapKey"],
        "k": "AQEBAQEBAQEBAQEBAQEBAQ"
    }))
    .unwrap();
    let wrong_keys = key::KeySet::new(vec![wrong_kek]);

    // Parsing with wrong KEK should fail during BIB verification
    let result = validate_with_keys(&signed_bytes, &wrong_keys);
    assert!(result.is_err(), "Verification with wrong KEK should fail");
}

#[test]
fn test_sign_then_encrypt() {
    // 1. Create a bundle
    let (_bundle, bundle_bytes) =
        Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_report_to("ipn:2.1".parse().unwrap())
            .with_lifetime(core::time::Duration::from_millis(1000))
            .with_payload(b"hello".as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();

    // Keys
    let sign_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "HS256",
        "key_ops": ["sign", "verify"],
        "k": "c2VjcmV0X3NpZ25pbmdfa2V5"
    }))
    .unwrap();
    let enc_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "A128KW",
        "enc": "A128GCM",
        "key_ops": ["encrypt", "decrypt", "wrapKey", "unwrapKey"],
        "k": "AAAAAAAAAAAAAAAAAAAAAA"
    }))
    .unwrap();
    let sign_keys = key::KeySet::new(vec![sign_key.clone()]);
    let enc_keys = key::KeySet::new(vec![enc_key.clone()]);
    let all_keys = key::KeySet::new(vec![sign_key.clone(), enc_key.clone()]);

    // 2. Sign
    let raw = raw_of(&bundle_bytes);
    let signer = signer::Signer::new(&raw, &bundle_bytes)
        .sign_block(
            1,
            signer::Context::HMAC_SHA2(ScopeFlags::default()),
            "ipn:2.1".parse().unwrap(),
            &sign_key,
        )
        .map_err(|(_, e)| e)
        .expect("Failed to sign block");
    let signed_bytes = signer.rebuild().expect("Failed to rebuild signed bundle");
    // println!("Bundle bytes: {:02x?}", signed_bytes);

    validate_with_keys(&signed_bytes, &sign_keys).expect("Failed to parse signed bundle");

    // 3. Encrypt
    // Exclude the security header from AAD to avoid mismatches due to BCB header mutation
    let flags = ScopeFlags {
        include_security_header: false,
        ..ScopeFlags::default()
    };

    let raw = raw_of(&signed_bytes);
    let encryptor = encryptor::Encryptor::new(&raw, &signed_bytes)
        .encrypt_block(
            1,
            encryptor::Context::AES_GCM(flags),
            "ipn:2.1".parse().unwrap(),
            &enc_key,
        )
        .map_err(|(_, e)| e)
        .expect("Failed to encrypt block");
    let encrypted_bytes = encryptor
        .rebuild()
        .expect("Failed to rebuild encrypted bundle");
    // println!("Bundle bytes: {:02x?}", encrypted_bytes);

    // 4. Decrypt and Verify
    let (encrypted_bytes, parsed_enc, bcb_ops, bib_ops) =
        validate_with_keys(&encrypted_bytes, &enc_keys).expect("Failed to parse encrypted bundle");
    // println!("{:#?}", parsed_enc);

    // Attempt to decrypt the BIB first to isolate decryption issues from verification issues
    if let Some(bib_num) = parsed_enc.blocks.get(&1).and_then(|b| match b.bib {
        hardy_bpv7::block::BibCoverage::Some(n) => Some(n),
        _ => None,
    }) {
        // println!("Found BIB at block {bib_num}");
        block_data(
            bib_num,
            &parsed_enc.blocks,
            &encrypted_bytes,
            &bcb_ops,
            &enc_keys,
        )
        .expect("BIB Decryption failed");
    }

    // This should succeed if everything is working
    verify_block(
        1,
        &parsed_enc.blocks,
        &encrypted_bytes,
        &bcb_ops,
        &bib_ops,
        &all_keys,
    )
    .expect("Verification failed");

    // Also check decryption of payload directly
    let payload = block_data(1, &parsed_enc.blocks, &encrypted_bytes, &bcb_ops, &enc_keys)
        .expect("Decryption failed");
    assert_eq!(payload.as_ref(), b"hello");
}

#[test]
fn test_rfc9173_decrypt_payload_leaves_bib_encrypted() {
    // RFC 9173 BCB-AES-GCM behavior:
    // Due to the IV uniqueness requirement (RFC 9173 Section 4.3.1), BCB-AES-GCM
    // cannot have multiple targets in a single BCB. Each encryption operation
    // requires a unique IV, so the encryptor creates SEPARATE BCBs for the
    // payload and the BIB.
    //
    // When decrypting the payload, only the payload's BCB is removed. The BIB
    // remains encrypted by its own BCB. This is expected behavior for RFC 9173.
    //
    // Future security contexts (e.g., COSE-based per draft-ietf-dtn-bpsec-cose)
    // may support multi-target BCBs with per-result IVs, which would allow
    // decrypting the payload to also decrypt the BIB in the same operation.

    // 1. Create a bundle
    let (_bundle, bundle_bytes) =
        Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_report_to("ipn:2.1".parse().unwrap())
            .with_lifetime(core::time::Duration::from_millis(1000))
            .with_payload(b"test payload data".as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();

    // Keys
    let sign_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "HS256",
        "key_ops": ["sign", "verify"],
        "k": "c2VjcmV0X3NpZ25pbmdfa2V5"
    }))
    .unwrap();
    let enc_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "A128KW",
        "enc": "A128GCM",
        "key_ops": ["encrypt", "decrypt", "wrapKey", "unwrapKey"],
        "k": "AAAAAAAAAAAAAAAAAAAAAA"
    }))
    .unwrap();
    let all_keys = key::KeySet::new(vec![sign_key.clone(), enc_key.clone()]);

    // 2. Sign payload (adds BIB)
    let raw = raw_of(&bundle_bytes);
    let signer = signer::Signer::new(&raw, &bundle_bytes)
        .sign_block(
            1,
            signer::Context::HMAC_SHA2(ScopeFlags::default()),
            "ipn:2.1".parse().unwrap(),
            &sign_key,
        )
        .map_err(|(_, e)| e)
        .expect("Failed to sign block");
    let signed_bytes = signer.rebuild().expect("Failed to rebuild signed bundle");

    validate_with_keys(&signed_bytes, &all_keys).expect("Failed to parse signed bundle");

    // 3. Encrypt payload with BCB-AES-GCM
    // Due to IV uniqueness requirements, this creates 2 SEPARATE BCBs:
    // one for the payload, one for the BIB
    let flags = ScopeFlags {
        include_security_header: false,
        ..ScopeFlags::default()
    };

    let raw = raw_of(&signed_bytes);
    let encryptor = encryptor::Encryptor::new(&raw, &signed_bytes)
        .encrypt_block(
            1,
            encryptor::Context::AES_GCM(flags),
            "ipn:2.1".parse().unwrap(),
            &enc_key,
        )
        .map_err(|(_, e)| e)
        .expect("Failed to encrypt block");
    let encrypted_bytes = encryptor
        .rebuild()
        .expect("Failed to rebuild encrypted bundle");

    let (encrypted_bytes, parsed_enc, _bcb_ops, _bib_ops) =
        validate_with_keys(&encrypted_bytes, &all_keys).expect("Failed to parse encrypted bundle");

    // Verify we have 2 BCB blocks (separate BCBs for payload and BIB)
    let bcb_count = count_blocks_of_type(&parsed_enc, hardy_bpv7::block::Type::BlockSecurity);
    assert_eq!(
        bcb_count, 2,
        "BCB-AES-GCM should create 2 separate BCBs (one for payload, one for BIB)"
    );

    // Verify we have 1 BIB block (encrypted by its own BCB)
    let bib_count = count_blocks_of_type(&parsed_enc, hardy_bpv7::block::Type::BlockIntegrity);
    assert_eq!(bib_count, 1, "Should have 1 BIB block");

    // 4. Remove BCB from payload only
    let raw = raw_of(&encrypted_bytes);
    let editor = bpsec::edit::remove_encryption(Editor::new(&raw, &encrypted_bytes), 1, &all_keys)
        .map_err(|(_, e)| e)
        .expect("Failed to remove BCB from payload");
    let decrypted_bytes = editor
        .rebuild()
        .map(|c| Chunk::flatten(c, &encrypted_bytes))
        .expect("Failed to rebuild after removing payload BCB");

    let (decrypted_bytes, parsed_decrypted, _bcb_ops, _bib_ops) =
        validate_with_keys(&decrypted_bytes, &all_keys).expect("Failed to parse decrypted bundle");

    // 5. Assert: 1 BCB remains (the BIB's BCB is still present)
    // This is expected RFC 9173 behavior - separate BCBs mean separate operations
    let bcb_count_after =
        count_blocks_of_type(&parsed_decrypted, hardy_bpv7::block::Type::BlockSecurity);
    assert_eq!(
        bcb_count_after, 1,
        "BIB's BCB should remain (RFC 9173 creates separate BCBs due to IV uniqueness)"
    );

    // 6. Assert: 1 BIB remains (still encrypted by its BCB)
    let bib_count_after =
        count_blocks_of_type(&parsed_decrypted, hardy_bpv7::block::Type::BlockIntegrity);
    assert_eq!(
        bib_count_after, 1,
        "BIB should remain encrypted (RFC 9173 creates separate BCBs)"
    );

    // 7. Verify payload is decrypted correctly
    let payload_block = parsed_decrypted
        .blocks
        .get(&1)
        .expect("Payload block missing");
    let payload_data = payload_block
        .payload(&decrypted_bytes)
        .expect("No payload data");
    assert_eq!(
        payload_data, b"test payload data",
        "Payload should be decrypted"
    );

    // 8. Verify payload does NOT have CRC (BIB provides integrity protection)
    assert!(
        matches!(payload_block.crc_type, hardy_bpv7::crc::CrcType::None),
        "Payload should not have CRC when BIB exists"
    );
}

#[test]
fn test_bib_removal_and_readd() {
    // 1. Create a bundle
    let (_bundle, bundle_bytes) =
        Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_report_to("ipn:2.1".parse().unwrap())
            .with_lifetime(core::time::Duration::from_millis(1000))
            .with_payload(b"test payload".as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();

    let sign_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "HS256",
        "key_ops": ["sign", "verify"],
        "k": "c2VjcmV0X3NpZ25pbmdfa2V5"
    }))
    .unwrap();
    let keys = key::KeySet::new(vec![sign_key.clone()]);

    // 2. Sign payload (adds BIB)
    let raw = raw_of(&bundle_bytes);
    let signer = signer::Signer::new(&raw, &bundle_bytes)
        .sign_block(
            1,
            signer::Context::HMAC_SHA2(ScopeFlags::default()),
            "ipn:2.1".parse().unwrap(),
            &sign_key,
        )
        .map_err(|(_, e)| e)
        .expect("Failed to sign block");
    let signed_bytes = signer.rebuild().expect("Failed to rebuild signed bundle");

    let (signed_bytes, parsed_signed, bcb_ops, bib_ops) =
        validate_with_keys(&signed_bytes, &keys).expect("Failed to parse signed bundle");

    // 3. Verify signature succeeds
    verify_block(
        1,
        &parsed_signed.blocks,
        &signed_bytes,
        &bcb_ops,
        &bib_ops,
        &keys,
    )
    .expect("Signature verification should succeed");

    let bib_count = count_blocks_of_type(&parsed_signed, hardy_bpv7::block::Type::BlockIntegrity);
    assert_eq!(bib_count, 1, "Should have 1 BIB after signing");

    // 4. Remove BIB using Editor::remove_integrity
    let raw = raw_of(&signed_bytes);
    let editor = Editor::new(&raw, &signed_bytes)
        .remove_integrity(1)
        .map_err(|(_, e)| e)
        .expect("Failed to remove BIB");
    let unsigned_bytes = editor
        .rebuild()
        .map(|c| Chunk::flatten(c, &signed_bytes))
        .expect("Failed to rebuild after BIB removal");

    let (unsigned_bytes, parsed_unsigned, bcb_ops, bib_ops) =
        validate_with_keys(&unsigned_bytes, &keys).expect("Failed to parse unsigned bundle");

    // 5. Assert: No BIB blocks exist
    let bib_count_after =
        count_blocks_of_type(&parsed_unsigned, hardy_bpv7::block::Type::BlockIntegrity);
    assert_eq!(bib_count_after, 0, "Should have 0 BIBs after removal");

    // 6. Verify signature fails (no BIB)
    let verify_result = verify_block(
        1,
        &parsed_unsigned.blocks,
        &unsigned_bytes,
        &bcb_ops,
        &bib_ops,
        &keys,
    )
    .expect("verify_block should not error");
    assert!(
        !verify_result,
        "Signature verification should return false when BIB is removed"
    );

    // 7. Re-sign payload
    let raw = raw_of(&unsigned_bytes);
    let signer = signer::Signer::new(&raw, &unsigned_bytes)
        .sign_block(
            1,
            signer::Context::HMAC_SHA2(ScopeFlags::default()),
            "ipn:2.1".parse().unwrap(),
            &sign_key,
        )
        .map_err(|(_, e)| e)
        .expect("Failed to re-sign block");
    let resigned_bytes = signer
        .rebuild()
        .expect("Failed to rebuild re-signed bundle");

    let (resigned_bytes, parsed_resigned, bcb_ops, bib_ops) =
        validate_with_keys(&resigned_bytes, &keys).expect("Failed to parse re-signed bundle");

    // 8. Verify signature succeeds again
    verify_block(
        1,
        &parsed_resigned.blocks,
        &resigned_bytes,
        &bcb_ops,
        &bib_ops,
        &keys,
    )
    .expect("Signature verification should succeed after re-signing");
}

#[test]
fn test_encrypt_then_sign_fails() {
    // This test demonstrates that you cannot sign an encrypted block
    // because the signer needs access to plaintext data

    // 1. Create a bundle
    let (_bundle, bundle_bytes) =
        Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_report_to("ipn:2.1".parse().unwrap())
            .with_lifetime(core::time::Duration::from_millis(1000))
            .with_payload(b"payload data".as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();

    let sign_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "HS256",
        "key_ops": ["sign", "verify"],
        "k": "c2VjcmV0X3NpZ25pbmdfa2V5"
    }))
    .unwrap();
    let enc_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "A128KW",
        "enc": "A128GCM",
        "key_ops": ["encrypt", "decrypt", "wrapKey", "unwrapKey"],
        "k": "AAAAAAAAAAAAAAAAAAAAAA"
    }))
    .unwrap();
    let all_keys = key::KeySet::new(vec![sign_key.clone(), enc_key.clone()]);

    // 2. Encrypt payload (adds BCB)
    let flags = ScopeFlags {
        include_security_header: false,
        ..ScopeFlags::default()
    };

    let raw = raw_of(&bundle_bytes);
    let encryptor = encryptor::Encryptor::new(&raw, &bundle_bytes)
        .encrypt_block(
            1,
            encryptor::Context::AES_GCM(flags),
            "ipn:2.1".parse().unwrap(),
            &enc_key,
        )
        .map_err(|(_, e)| e)
        .expect("Failed to encrypt block");
    let encrypted_bytes = encryptor
        .rebuild()
        .expect("Failed to rebuild encrypted bundle");

    validate_with_keys(&encrypted_bytes, &all_keys).expect("Failed to parse encrypted bundle");

    // 3. Attempt to sign encrypted payload - this should fail
    let raw = raw_of(&encrypted_bytes);
    let sign_result = signer::Signer::new(&raw, &encrypted_bytes).sign_block(
        1,
        signer::Context::HMAC_SHA2(ScopeFlags::default()),
        "ipn:2.1".parse().unwrap(),
        &sign_key,
    );

    // Should fail because block 1 is encrypted
    assert!(
        sign_result.is_err(),
        "Signing an encrypted block should fail"
    );
}

#[test]
fn test_signature_tamper_detection() {
    // 1. Create and sign bundle
    let (_bundle, bundle_bytes) =
        Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_report_to("ipn:2.1".parse().unwrap())
            .with_lifetime(core::time::Duration::from_millis(1000))
            .with_payload(b"original payload".as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();

    let sign_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "HS256",
        "key_ops": ["sign", "verify"],
        "k": "c2VjcmV0X3NpZ25pbmdfa2V5"
    }))
    .unwrap();
    let keys = key::KeySet::new(vec![sign_key.clone()]);

    let raw = raw_of(&bundle_bytes);
    let signer = signer::Signer::new(&raw, &bundle_bytes)
        .sign_block(
            1,
            signer::Context::HMAC_SHA2(ScopeFlags::default()),
            "ipn:2.1".parse().unwrap(),
            &sign_key,
        )
        .map_err(|(_, e)| e)
        .expect("Failed to sign block");
    let signed_bytes = signer.rebuild().expect("Failed to rebuild signed bundle");

    let (signed_bytes, parsed_signed, bcb_ops, bib_ops) =
        validate_with_keys(&signed_bytes, &keys).expect("Failed to parse signed bundle");

    // Verify signature succeeds with untampered bundle
    verify_block(
        1,
        &parsed_signed.blocks,
        &signed_bytes,
        &bcb_ops,
        &bib_ops,
        &keys,
    )
    .expect("Signature verification should succeed on untampered bundle");

    // 2. Manually corrupt a byte in the payload DATA (not CBOR structure)
    let mut tampered_bytes = signed_bytes.to_vec();

    // Get the payload range and corrupt the last byte
    let payload_block = parsed_signed.blocks.get(&1).expect("Payload block missing");
    let payload_range = payload_block.payload_range();
    // Corrupt the last byte of the payload data
    tampered_bytes[payload_range.end as usize - 1] ^= 0xFF;

    // 3. Parsing should fail with IntegrityCheckFailed since verification happens during parsing
    let parse_result = validate_with_keys(&tampered_bytes, &keys);
    assert!(
        matches!(
            parse_result,
            Err(hardy_bpv7::Error::InvalidBPSec(
                hardy_bpv7::bpsec::Error::IntegrityCheckFailed
            ))
        ),
        "Tampered bundle should fail to parse with IntegrityCheckFailed, got error: {:?}",
        parse_result.as_ref().err()
    );
}

#[test]
fn test_bcb_without_bib_removal() {
    // 1. Create bundle
    let (_bundle, bundle_bytes) =
        Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_report_to("ipn:2.1".parse().unwrap())
            .with_lifetime(core::time::Duration::from_millis(1000))
            .with_payload(b"encrypted data".as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();

    let enc_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "A128KW",
        "enc": "A128GCM",
        "key_ops": ["encrypt", "decrypt", "wrapKey", "unwrapKey"],
        "k": "AAAAAAAAAAAAAAAAAAAAAA"
    }))
    .unwrap();
    let keys = key::KeySet::new(vec![enc_key.clone()]);

    // 2. Encrypt payload only (no signing, just BCB)
    let flags = ScopeFlags {
        include_security_header: false,
        ..ScopeFlags::default()
    };

    let raw = raw_of(&bundle_bytes);
    let encryptor = encryptor::Encryptor::new(&raw, &bundle_bytes)
        .encrypt_block(
            1,
            encryptor::Context::AES_GCM(flags),
            "ipn:2.1".parse().unwrap(),
            &enc_key,
        )
        .map_err(|(_, e)| e)
        .expect("Failed to encrypt block");
    let encrypted_bytes = encryptor
        .rebuild()
        .expect("Failed to rebuild encrypted bundle");

    let (encrypted_bytes, parsed_enc, _bcb_ops, _bib_ops) =
        validate_with_keys(&encrypted_bytes, &keys).expect("Failed to parse encrypted bundle");

    // Verify BCB exists
    let bcb_count = count_blocks_of_type(&parsed_enc, hardy_bpv7::block::Type::BlockSecurity);
    assert_eq!(bcb_count, 1, "Should have 1 BCB after encryption");

    // 3. Remove BCB using Editor::remove_encryption
    let raw = raw_of(&encrypted_bytes);
    let editor = bpsec::edit::remove_encryption(Editor::new(&raw, &encrypted_bytes), 1, &keys)
        .map_err(|(_, e)| e)
        .expect("Failed to remove BCB");
    let decrypted_bytes = editor
        .rebuild()
        .map(|c| Chunk::flatten(c, &encrypted_bytes))
        .expect("Failed to rebuild after BCB removal");

    let (decrypted_bytes, parsed_decrypted, _bcb_ops, _bib_ops) =
        validate_with_keys(&decrypted_bytes, &keys).expect("Failed to parse decrypted bundle");

    // 4. Assert: 0 BCBs, payload is decrypted
    let bcb_count_after =
        count_blocks_of_type(&parsed_decrypted, hardy_bpv7::block::Type::BlockSecurity);
    assert_eq!(bcb_count_after, 0, "Should have 0 BCBs after removal");

    // 5. Payload content matches original
    let payload_block = parsed_decrypted
        .blocks
        .get(&1)
        .expect("Payload block missing");
    let payload_data = payload_block
        .payload(&decrypted_bytes)
        .expect("No payload data");
    assert_eq!(
        payload_data, b"encrypted data",
        "Payload should match original after decryption"
    );
}

#[test]
fn test_remove_encryption_fails_on_unencrypted_block() {
    // Test that remove_encryption returns NotEncrypted error when called on a block
    // that is not the target of a BCB

    let keys: key::KeySet = serde_json::from_value(serde_json::json!({
        "keys": [{
            "kid": "ipn:2.1",
            "kty": "oct",
            "alg": "A128KW",
            "enc": "A128GCM",
            "key_ops": ["wrapKey", "encrypt", "unwrapKey", "decrypt"],
            "k": "YWJjZGVmZ2hpamtsbW5vcA"
        }]
    }))
    .unwrap();

    // Create a simple bundle with no encryption
    let (bundle, bundle_bytes) =
        Builder::new("ipn:1.1".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_payload(b"not encrypted".as_slice().into())
            .build(CreationTimestamp::now())
            .expect("Failed to build bundle");

    // Verify no BCBs exist (use the raw parse for inspection; the rich
    // `bundle` from Builder is consumed by Editor below).
    let raw = raw_of(&bundle_bytes);
    let bcb_count = count_blocks_of_type(&raw, hardy_bpv7::block::Type::BlockSecurity);
    assert_eq!(bcb_count, 0, "Should have 0 BCBs (bundle is not encrypted)");
    let _ = bundle;

    // Attempt to remove encryption from payload block (which is not encrypted)
    let result = bpsec::edit::remove_encryption(Editor::new(&raw, &bundle_bytes), 1, &keys);

    // Should fail with NotEncrypted error
    let Err((_, e)) = result else {
        panic!("Expected remove_encryption to fail on unencrypted block");
    };
    assert!(
        e.to_string().contains("not the target of a BCB"),
        "Expected NotEncrypted error, got: {}",
        e
    );
}

#[test]
fn test_remove_integrity_fails_on_unsigned_block() {
    // Test that remove_integrity returns NotSigned error when called on a block
    // that is not the target of a BIB

    // Create a simple bundle with no integrity protection (no BIB)
    let (bundle, bundle_bytes) =
        Builder::new("ipn:1.1".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_payload(b"not signed".as_slice().into())
            .build(CreationTimestamp::now())
            .expect("Failed to build bundle");

    // Verify no BIBs exist (use the raw parse for inspection).
    let raw = raw_of(&bundle_bytes);
    let bib_count = count_blocks_of_type(&raw, hardy_bpv7::block::Type::BlockIntegrity);
    assert_eq!(bib_count, 0, "Should have 0 BIBs (bundle is not signed)");
    let _ = bundle;

    // Attempt to remove integrity from payload block (which is not signed)
    let result = Editor::new(&raw, &bundle_bytes).remove_integrity(1);

    // Should fail with NotSigned error
    let Err((_, e)) = result else {
        panic!("Expected remove_integrity to fail on unsigned block");
    };
    assert!(
        e.to_string().contains("not the target of a BIB"),
        "Expected NotSigned error, got: {}",
        e
    );
}

#[test]
fn test_encrypt_bib_directly_fails() {
    // Test that attempting to directly encrypt a BIB block fails.
    // RFC 9172 Section 3.8: A BCB MUST NOT target a BIB unless it shares a security target.
    // BIBs should only be encrypted as a side-effect when encrypting a block they protect.

    // 1. Create a bundle
    let (_bundle, bundle_bytes) =
        Builder::new("ipn:1.1".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_payload(b"test payload".as_slice().into())
            .build(CreationTimestamp::now())
            .expect("Failed to build bundle");

    // 2. Sign the payload (creates a BIB)
    let sign_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "HS256",
        "key_ops": ["sign", "verify"],
        "k": "c2VjcmV0X3NpZ25pbmdfa2V5"
    }))
    .unwrap();

    let raw = raw_of(&bundle_bytes);
    let signer = signer::Signer::new(&raw, &bundle_bytes)
        .sign_block(
            1,
            signer::Context::HMAC_SHA2(ScopeFlags::default()),
            "ipn:2.1".parse().unwrap(),
            &sign_key,
        )
        .map_err(|(_, e)| e)
        .expect("Failed to sign block");
    let signed_bytes = signer.rebuild().expect("Failed to rebuild signed bundle");

    let sign_keys = key::KeySet::new(vec![sign_key]);
    let (signed_bytes, parsed_signed, _bcb_ops, _bib_ops) =
        validate_with_keys(&signed_bytes, &sign_keys).expect("Failed to parse signed bundle");

    // 3. Find the BIB block number
    let bib_block_num = parsed_signed
        .blocks
        .get(&1)
        .and_then(|b| match b.bib {
            hardy_bpv7::block::BibCoverage::Some(n) => Some(n),
            _ => None,
        })
        .expect("BIB not found on payload block");

    // 4. Attempt to directly encrypt the BIB - this should fail
    let enc_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "A128KW",
        "enc": "A128GCM",
        "key_ops": ["encrypt", "decrypt", "wrapKey", "unwrapKey"],
        "k": "AAAAAAAAAAAAAAAAAAAAAA"
    }))
    .unwrap();

    let raw = raw_of(&signed_bytes);
    let result = encryptor::Encryptor::new(&raw, &signed_bytes).encrypt_block(
        bib_block_num,
        encryptor::Context::AES_GCM(ScopeFlags::default()),
        "ipn:2.1".parse().unwrap(),
        &enc_key,
    );

    // Should fail with InvalidTarget error
    let Err((_, e)) = result else {
        panic!("Expected encrypt_block to fail when directly targeting a BIB");
    };
    assert!(
        e.to_string().contains("Invalid block target"),
        "Expected InvalidTarget error, got: {}",
        e
    );
}

#[test]
fn test_sign_primary_block_with_crc() {
    // Test that signing the primary block (block 0) works even when
    // the primary block has a CRC. RFC 9171 Section 4.3.1 allows
    // both CRC and BIB on the primary block.

    // 1. Create a bundle (primary block will have a CRC by default)
    let (bundle, bundle_bytes) =
        Builder::new("ipn:1.1".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_payload(b"test payload".as_slice().into())
            .build(CreationTimestamp::now())
            .expect("Failed to build bundle");

    // Verify primary block has a CRC
    let primary = bundle.blocks.get(&0).expect("Primary block missing");
    assert!(
        !matches!(primary.crc_type, hardy_bpv7::crc::CrcType::None),
        "Primary block should have a CRC"
    );

    // 2. Sign the primary block (block 0)
    let sign_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "HS256",
        "key_ops": ["sign", "verify"],
        "k": "c2VjcmV0X3NpZ25pbmdfa2V5"
    }))
    .unwrap();

    let raw = raw_of(&bundle_bytes);
    let signer = signer::Signer::new(&raw, &bundle_bytes)
        .sign_block(
            0, // Sign the primary block
            signer::Context::HMAC_SHA2(ScopeFlags::default()),
            "ipn:2.1".parse().unwrap(),
            &sign_key,
        )
        .map_err(|(_, e)| e)
        .expect("Failed to sign primary block");

    let signed_bytes = signer
        .rebuild()
        .expect("Failed to rebuild bundle after signing primary block");

    // 3. Parse and verify the signed bundle
    let keys = key::KeySet::new(vec![sign_key]);
    let (signed_bytes, parsed, bcb_ops, bib_ops) =
        validate_with_keys(&signed_bytes, &keys).expect("Failed to parse signed bundle");

    // 4. Verify BIB exists and targets block 0
    let bib_count = count_blocks_of_type(&parsed, hardy_bpv7::block::Type::BlockIntegrity);
    assert_eq!(
        bib_count, 1,
        "Should have 1 BIB after signing primary block"
    );

    // 5. Verify the primary block still has its CRC (RFC 9171 allows this)
    let signed_primary = parsed.blocks.get(&0).expect("Primary block missing");
    assert!(
        !matches!(signed_primary.crc_type, hardy_bpv7::crc::CrcType::None),
        "Primary block should still have CRC after signing"
    );

    // 6. Verify the signature
    verify_block(0, &parsed.blocks, &signed_bytes, &bcb_ops, &bib_ops, &keys)
        .expect("Failed to verify signature on primary block");
}

#[test]
fn test_sign_primary_block_with_crc_no_scope_flags() {
    // Test signing primary block with ScopeFlags::NONE to ensure
    // CRC handling works regardless of AAD configuration.

    let (_bundle, bundle_bytes) =
        Builder::new("ipn:1.1".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_payload(b"test payload".as_slice().into())
            .build(CreationTimestamp::now())
            .expect("Failed to build bundle");

    let sign_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "HS256",
        "key_ops": ["sign", "verify"],
        "k": "c2VjcmV0X3NpZ25pbmdfa2V5"
    }))
    .unwrap();

    // Use ScopeFlags::NONE - no AAD included
    let raw = raw_of(&bundle_bytes);
    let signer = signer::Signer::new(&raw, &bundle_bytes)
        .sign_block(
            0,
            signer::Context::HMAC_SHA2(ScopeFlags::NONE),
            "ipn:2.1".parse().unwrap(),
            &sign_key,
        )
        .map_err(|(_, e)| e)
        .expect("Failed to sign primary block with NONE flags");

    let signed_bytes = signer
        .rebuild()
        .expect("Failed to rebuild bundle after signing primary block with NONE flags");

    let keys = key::KeySet::new(vec![sign_key]);
    let (signed_bytes, parsed, bcb_ops, bib_ops) =
        validate_with_keys(&signed_bytes, &keys).expect("Failed to parse signed bundle");

    // Verify signature works with NONE flags
    verify_block(0, &parsed.blocks, &signed_bytes, &bcb_ops, &bib_ops, &keys)
        .expect("Failed to verify signature on primary block with NONE flags");
}

#[test]
fn test_sign_removes_crc_from_target_block() {
    // Test that signing a block properly removes the CRC from the target block
    // (not just setting the type to None while keeping the CRC value)

    // 1. Create a bundle with CRC on payload block
    let (bundle, bundle_bytes) =
        Builder::new("ipn:1.1".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_payload(b"test payload".as_slice().into())
            .build(CreationTimestamp::now())
            .expect("Failed to build bundle");

    // Verify payload block (block 1) has a CRC before signing
    let payload_block = bundle.blocks.get(&1).expect("Payload block missing");
    assert!(
        !matches!(payload_block.crc_type, hardy_bpv7::crc::CrcType::None),
        "Payload block should have a CRC before signing"
    );

    // 2. Sign the payload block
    let sign_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "HS256",
        "key_ops": ["sign", "verify"],
        "k": "c2VjcmV0X3NpZ25pbmdfa2V5"
    }))
    .unwrap();

    let raw = raw_of(&bundle_bytes);
    let signer = signer::Signer::new(&raw, &bundle_bytes)
        .sign_block(
            1,
            signer::Context::HMAC_SHA2(ScopeFlags::default()),
            "ipn:2.1".parse().unwrap(),
            &sign_key,
        )
        .map_err(|(_, e)| e)
        .expect("Failed to sign payload block");

    let signed_bytes = signer
        .rebuild()
        .expect("Failed to rebuild bundle after signing");

    // 3. Parse the signed bundle and verify CRC is removed from payload block
    let keys = key::KeySet::new(vec![sign_key]);
    let (signed_bytes, parsed, bcb_ops, bib_ops) =
        validate_with_keys(&signed_bytes, &keys).expect("Failed to parse signed bundle");

    let signed_payload = parsed.blocks.get(&1).expect("Payload block missing");
    assert!(
        matches!(signed_payload.crc_type, hardy_bpv7::crc::CrcType::None),
        "Payload block CRC type should be None after signing, got {:?}",
        signed_payload.crc_type
    );

    // 4. Verify the signature still works
    verify_block(1, &parsed.blocks, &signed_bytes, &bcb_ops, &bib_ops, &keys)
        .expect("Failed to verify signature on payload block");

    // 5. Verify the payload block has 5 elements (no CRC) by checking raw CBOR
    // The payload block should be a CBOR array with 5 elements when CRC type is None
    // Find the payload block extent and check its structure
    let payload_extent = signed_payload.extent.start as usize..signed_payload.extent.end as usize;
    let payload_cbor = &signed_bytes[payload_extent];

    // First byte should be 0x85 (array of 5 elements) not 0x86 (array of 6 elements)
    assert_eq!(
        payload_cbor[0], 0x85,
        "Payload block CBOR should be array of 5 elements (0x85), got 0x{:02x}",
        payload_cbor[0]
    );
}
