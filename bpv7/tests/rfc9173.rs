use core::time::Duration;
use hardy_bpv7::{
    Bundle,
    block::{BibCoverage, Block, Flags, Type},
    bpsec::{
        self, Decrypted, SecurityService, bcb, bib, checks::Checked, edit::BPSecEditor, encryptor,
        key, rfc9173::ScopeFlags, signer,
    },
    builder::{self, Builder},
    crc,
    creation_timestamp::CreationTimestamp,
    editor::{self, Chunk, Editor},
    parse,
    reader::PlainReader,
};
use std::collections::{HashMap, HashSet};
use zeroize::Zeroizing;

mod common;
use self::common::{accept_all, rand_k};
// Helper function to count blocks of a specific type
fn count_blocks_of_type(bundle: &Bundle, block_type: Type) -> usize {
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

/// Structural parse + keyed BPSec validation: §A, the composed keyed
/// verification, then every BIB verified over the bundle as its destination
/// reads it (every block's encryption removed with `keys`). Returns the
/// parser-owned `Bytes` plus the bundle and decoded BPSec OperationSets of
/// the structural parse. NoKey on a BIB is soft.
#[allow(clippy::type_complexity)]
fn validate_with_keys(
    data: &[u8],
    keys: &key::KeySet,
) -> Result<
    (
        ::bytes::Bytes,
        Bundle,
        HashMap<u64, bpsec::bcb::OperationSet>,
        HashMap<u64, bpsec::bib::OperationSet>,
    ),
    hardy_bpv7::Error,
> {
    let mut parsed = parse::parse(::bytes::Bytes::copy_from_slice(data))?;

    // §A — classify: a block demanding the bundle's deletion is the error.
    if let Some(error) = parsed.classification.delete_bundle.take() {
        return Err(error);
    }

    // The composed keyed verification; the test helper is not a Verifier,
    // so a failed decryption or integrity check is rejected.
    let facts = common::verify_as_receiver(&mut parsed, keys, &HashSet::new())?;
    let failed = |service| facts.targets_of(service, Checked::Failed).next().is_some();
    if failed(SecurityService::Confidentiality) {
        return Err(bpsec::Error::DecryptionFailed.into());
    }
    if failed(SecurityService::Integrity) {
        return Err(bpsec::Error::IntegrityCheckFailed.into());
    }

    // Every BIB, an encrypted one included, verified over the plaintext.
    let accepted = accept_all(&parsed, keys).map_err(|e| match e {
        editor::Error::Bpsec(e) => e.into(),
        editor::Error::Builder(builder::Error::InternalError(e)) => e,
        e => panic!("removing the encryption failed outside BPSec: {e}"),
    })?;
    common::verify_all_bibs(&accepted.data, keys)?;

    Ok((parsed.data, parsed.bundle, parsed.bcbs, parsed.bibs))
}

/// Per-block BIB verify. Returns `Ok(true)` when the block was
/// BIB-covered and verified, `Ok(false)` when it had no BIB, and
/// `Err(_)` for any verify failure (including `NoKey`). Handles
/// BCB-encrypted targets transparently via `DecryptingReader`'s
/// on-demand decryption — RFC 9172 §3.10 sign-before-encrypt.
fn verify_block(
    block_number: u64,
    blocks: &HashMap<u64, Block>,
    data: &[u8],
    bcb_ops: &HashMap<u64, bpsec::bcb::OperationSet>,
    bib_ops: &HashMap<u64, bpsec::bib::OperationSet>,
    keys: &key::KeySet,
) -> Result<bool, hardy_bpv7::Error> {
    let target = blocks
        .get(&block_number)
        .ok_or(hardy_bpv7::Error::MissingBlock(block_number))?;
    let bib_block_number = match target.bib {
        hardy_bpv7::block::BibCoverage::Some(n) => n,
        hardy_bpv7::block::BibCoverage::None => return Ok(false),
        // Every BIB the keys decrypt is plaintext by here, so a coverage
        // still unresolved is a BIB the keys cannot decrypt.
        hardy_bpv7::block::BibCoverage::Maybe => {
            return Err(hardy_bpv7::Error::InvalidBPSec(bpsec::Error::NoKey));
        }
    };
    let opset = bib_ops
        .get(&bib_block_number)
        .ok_or(hardy_bpv7::Error::Altered)?;
    let op = opset
        .operations()
        .get(&block_number)
        .ok_or(hardy_bpv7::Error::Altered)?;
    let block_set = bpsec::DecryptingReader::new(blocks, data, bcb_ops, keys);
    op.verify(
        keys,
        bpsec::bib::OperationArgs {
            source: opset.source(),
            target: block_number,
            security_block: bib_block_number,
            blocks: &block_set,
        },
    )
    .map(|_| true)
    .map_err(hardy_bpv7::Error::InvalidBPSec)
}

/// Per-block plaintext: slice when unencrypted, BCB-decrypt when not.
fn block_data<'a>(
    block_number: u64,
    blocks: &'a HashMap<u64, Block>,
    data: &'a [u8],
    bcb_ops: &'a HashMap<u64, bpsec::bcb::OperationSet>,
    keys: &'a key::KeySet,
) -> Result<hardy_bpv7::block::Payload<'a>, hardy_bpv7::Error> {
    bpsec::DecryptingReader::new(blocks, data, bcb_ops, keys)
        .block_data(block_number)?
        .ok_or(hardy_bpv7::Error::Altered)
}

#[test]
fn rfc9173_appendix_a_1() {
    // RFC 9173 Appendix A.1.4's final bundle, with the Bundle Age block
    // RFC 9171 §4.4.2 requires of its zero creation time inserted before the
    // payload, where Appendix A.3 places one: no security result covers it,
    // so every result is the RFC's. No CRC on the primary block: the BPA's
    // gate checks that.
    let data = hex_literal::hex!(
        "9f88070000820282010282028202018202820201820018281a000f4240850b0200
                005856810101018202820201828201078203008181820158403bdc69b3a34a2b5d3a
                8554368bd1e808f606219d2a10a846eae3886ae4ecc83c4ee550fdfb1cc636b904e2
                f1a73e303dcd4b6ccece003e95e8164dcc89a156e185070300004319012c85010100
                005823526561647920746f2067656e657261746520612033322d6279746520706179
                6c6f6164ff"
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
    // RFC 9173 Appendix A.2.4's final bundle, with the Bundle Age block
    // RFC 9171 §4.4.2 requires of its zero creation time inserted before the
    // payload, where Appendix A.3 places one: no security result covers it,
    // so every result is the RFC's. No CRC on the primary block: the BPA's
    // gate checks that.
    let data = hex_literal::hex!(
        "9f88070000820282010282028202018202820201820018281a000f4240850c0201
                0058508101020182028202018482014c5477656c7665313231323132820201820358
                1869c411276fecddc4780df42c8a2af89296fabf34d7fae7008204008181820150ef
                a4b5ac0108e3816c5606479801bc0485070300004319012c850101000058233a09c1
                e63fe23a7f66a59c7303837241e070b02619fc59c5214a22f08cd70795e73e9aff"
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

// The payload's decrypt stage, begun on the headers alone and fed the
// received payload in runs, recovers RFC 9173's plaintext from Appendix
// A.2's bundle (a wrapped key, the tag carried as the BCB's result) and
// A.4's (a direct key, one BCB over the payload and the encrypted BIB).
#[test]
fn rfc9173_payloads_decrypt_as_they_stream() {
    // RFC 9173 Appendix A.1.1.2: "Ready to generate a 32-byte payload".
    const PLAINTEXT: &[u8] = b"Ready to generate a 32-byte payload";
    let a2 = hex_literal::hex!(
        "9f88070000820282010282028202018202820201820018281a000f4240850c0201
                0058508101020182028202018482014c5477656c7665313231323132820201820358
                1869c411276fecddc4780df42c8a2af89296fabf34d7fae7008204008181820150ef
                a4b5ac0108e3816c5606479801bc0485070300004319012c850101000058233a09c1
                e63fe23a7f66a59c7303837241e070b02619fc59c5214a22f08cd70795e73e9aff"
    );
    let a2_keys: key::KeySet = serde_json::from_value(serde_json::json!({
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
    let a4 = hex_literal::hex!(
        "9f88070000820282010282028202018202820201820018281a000f4240850b0300
                005846438ed6208eb1c1ffb94d952175167df0902902064a2983910c4fb2340790bf
                420a7d1921d5bf7c4721e02ab87a93ab1e0b75cf62e4948727c8b5dae46ed2af0543
                9b88029191850c0201005849820301020182028202018382014c5477656c76653132
                313231328202038204078281820150220ffc45c8a901999ecc60991dd78b29818201
                50d2c51cb2481792dae8b21d848cede99b85070400004319012c8501010000582390
                eab6457593379298a8724e16e61f837488e127212b59ac91f8a86287b7d07630a122
                ff"
    );
    let a4_keys: key::KeySet = serde_json::from_value(serde_json::json!({
        "keys": [{
            "kid": "ipn:2.1",
            "kty": "oct",
            "enc": "A256GCM",
            "key_ops": ["decrypt"],
            "k": "cXdlcnR5dWlvcGFzZGZnaHF3ZXJ0eXVpb3Bhc2RmZ2g"
        }]
    }))
    .unwrap();

    for (name, data, keys) in [("A.2", &a2[..], a2_keys), ("A.4", &a4[..], a4_keys)] {
        let parsed = parse::parse(::bytes::Bytes::copy_from_slice(data)).unwrap();
        let payload = parsed.bundle.blocks[&1].payload_range();
        let headers = &data[..usize::try_from(payload.start).unwrap()];
        let received =
            &data[usize::try_from(payload.start).unwrap()..usize::try_from(payload.end).unwrap()];

        let bcb = parsed.bundle.blocks[&1]
            .bcb
            .expect("a BCB covers the payload");
        let ops = &parsed.bcbs[&bcb];
        let mut decryptor = ops.operations()[&1]
            .begin_decrypt(
                &keys,
                &bcb::OperationArgs {
                    source: ops.source(),
                    target: 1,
                    security_block: bcb,
                    blocks: &PlainReader {
                        blocks: &parsed.bundle.blocks,
                        source_data: headers,
                    },
                },
                received.len() as u64,
            )
            .unwrap_or_else(|e| panic!("{name}: the decryption begins: {e}"));
        assert_eq!(decryptor.plaintext_len(), PLAINTEXT.len() as u64, "{name}");
        for run in received.chunks(7) {
            decryptor.update(run);
        }
        let plaintext = decryptor
            .finish()
            .unwrap_or_else(|e| panic!("{name}: the payload decrypts: {e}"));
        assert_eq!(plaintext.as_ref(), PLAINTEXT, "{name}");
    }
}

// The final bundle of RFC 9173 Appendix A.3: a BCB over the payload, and a
// BIB over the primary block (0) and the Bundle Age block (2).
const APPENDIX_A_3: [u8; 239] = hex_literal::hex!(
    "9f88070000820282010282028202018202820201820018281a000f4240850b0300
            00585c8200020101820282030082820105820300828182015820cac6ce8e4c5dae57
            988b757e49a6dd1431dc04763541b2845098265bc817241b81820158203ed614c0d9
            7f49b3633627779aa18a338d212bf3c92b97759d9739cd50725596850c0401005834
            8101020182028202018382014c5477656c7665313231323132820201820400818182
            0150efa4b5ac0108e3816c5606479801bc0485070200004319012c85010100005823
            3a09c1e63fe23a7f66a59c7303837241e070b02619fc59c5214a22f08cd70795e73e
            9aff"
);

#[test]
fn rfc9173_appendix_a_3() {
    let data = APPENDIX_A_3;
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
    // RFC 9173 Appendix A.4.5's final bundle, with the Bundle Age block
    // RFC 9171 §4.4.2 requires of its zero creation time inserted before the
    // payload, where Appendix A.3 places one: no security result covers it,
    // so every result is the RFC's. No CRC on the primary block: the BPA's
    // gate checks that.
    let data = hex_literal::hex!(
        "9f88070000820282010282028202018202820201820018281a000f4240850b0300
                005846438ed6208eb1c1ffb94d952175167df0902902064a2983910c4fb2340790bf
                420a7d1921d5bf7c4721e02ab87a93ab1e0b75cf62e4948727c8b5dae46ed2af0543
                9b88029191850c0201005849820301020182028202018382014c5477656c76653132
                313231328202038204078281820150220ffc45c8a901999ecc60991dd78b29818201
                50d2c51cb2481792dae8b21d848cede99b85070400004319012c8501010000582390
                eab6457593379298a8724e16e61f837488e127212b59ac91f8a86287b7d07630a122
                ff"
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

    let (data, raw, bcb_ops, _) = validate_with_keys(&data, &keys).unwrap();
    block_data(1, &raw.blocks, &data, &bcb_ops, &keys).expect("Failed to decrypt");
    // The payload's BIB is encrypted: it is verified as the destination reads it.
    let accepted = accept_all(&parse::parse(data).unwrap(), &keys).unwrap();
    verify_block(
        1,
        &accepted.bundle.blocks,
        &accepted.data,
        &accepted.bcbs,
        &accepted.bibs,
        &keys,
    )
    .expect("Failed to verify");
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
        "k": rand_k(16)
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

    // Two distinct KEKs: signing uses one, verification the other.
    let kek_k = rand_k(16);
    let wrong_k = rand_k(16);
    assert_ne!(
        kek_k, wrong_k,
        "the wrong KEK must differ from the signing KEK"
    );

    let sign_kek: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "HS256+A128KW",
        "key_ops": ["sign", "wrapKey"],
        "k": kek_k
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
        "k": wrong_k
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
            .with_lifetime(Duration::from_millis(1000))
            .with_payload(b"hello".as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();

    // Keys
    let sign_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "HS256",
        "key_ops": ["sign", "verify"],
        "k": rand_k(18)
    }))
    .unwrap();
    let enc_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "A128KW",
        "enc": "A128GCM",
        "key_ops": ["encrypt", "decrypt", "wrapKey", "unwrapKey"],
        "k": rand_k(16)
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
    let (encrypted_bytes, parsed_enc, bcb_ops, _) =
        validate_with_keys(&encrypted_bytes, &enc_keys).expect("Failed to parse encrypted bundle");

    // Decrypt the BIB first, to isolate decryption issues from verification
    // issues.
    let bib_num = parsed_enc
        .blocks
        .iter()
        .find_map(|(&n, b)| (b.block_type == Type::BlockIntegrity).then_some(n))
        .expect("the BIB is present");
    block_data(
        bib_num,
        &parsed_enc.blocks,
        &encrypted_bytes,
        &bcb_ops,
        &enc_keys,
    )
    .expect("BIB Decryption failed");

    // The payload's BIB is encrypted: it is verified as the destination
    // reads it.
    let accepted = accept_all(&parse::parse(encrypted_bytes.clone()).unwrap(), &enc_keys).unwrap();
    verify_block(
        1,
        &accepted.bundle.blocks,
        &accepted.data,
        &accepted.bcbs,
        &accepted.bibs,
        &all_keys,
    )
    .expect("Verification failed");

    // Also check decryption of payload directly
    let payload = block_data(1, &parsed_enc.blocks, &encrypted_bytes, &bcb_ops, &enc_keys)
        .expect("Decryption failed");
    assert_eq!(payload.as_ref(), b"hello");
}

// Removing the payload's encryption removes the encryption of the BIB over
// it too, all or none: the BIB stays, in plaintext, with its operation.
#[test]
fn test_rfc9173_decrypt_payload_takes_its_encrypted_bib() {
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
            .with_lifetime(Duration::from_millis(1000))
            .with_payload(b"test payload data".as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();

    // Keys
    let sign_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "HS256",
        "key_ops": ["sign", "verify"],
        "k": rand_k(18)
    }))
    .unwrap();
    let enc_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "A128KW",
        "enc": "A128GCM",
        "key_ops": ["encrypt", "decrypt", "wrapKey", "unwrapKey"],
        "k": rand_k(16)
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
    let bcb_count = count_blocks_of_type(&parsed_enc, Type::BlockSecurity);
    assert_eq!(
        bcb_count, 2,
        "BCB-AES-GCM should create 2 separate BCBs (one for payload, one for BIB)"
    );

    // Verify we have 1 BIB block (encrypted by its own BCB)
    let bib_count = count_blocks_of_type(&parsed_enc, Type::BlockIntegrity);
    assert_eq!(bib_count, 1, "Should have 1 BIB block");

    // 4. Remove the payload's BCB
    let raw = raw_of(&encrypted_bytes);
    let editor = Editor::new(&raw, &encrypted_bytes)
        .with_key_source(&all_keys)
        .remove_encryption(1)
        .map_err(|(_, e)| e)
        .expect("Failed to remove BCB from payload");
    let decrypted_bytes = editor
        .rebuild()
        .map(|c| Chunk::flatten(c, &encrypted_bytes))
        .expect("Failed to rebuild after removing payload BCB");

    let (decrypted_bytes, parsed_decrypted, _bcb_ops, _bib_ops) =
        validate_with_keys(&decrypted_bytes, &all_keys).expect("Failed to parse decrypted bundle");

    // 5. Assert: no BCB remains; the BIB's went with the payload's
    let bcb_count_after = count_blocks_of_type(&parsed_decrypted, Type::BlockSecurity);
    assert_eq!(bcb_count_after, 0, "the BIB's BCB goes with the payload's");

    // 6. Assert: the BIB remains, in plaintext, over the payload
    let bib = parsed_decrypted
        .blocks
        .iter()
        .find_map(|(&n, b)| (b.block_type == Type::BlockIntegrity).then_some(n))
        .expect("the BIB remains");
    assert!(parsed_decrypted.blocks[&bib].bcb.is_none());
    assert_eq!(parsed_decrypted.blocks[&1].bib, BibCoverage::Some(bib));

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

    // 8. The payload keeps the CRC it was built with: no security operation
    // changes a target's CRC.
    assert_eq!(
        payload_block.crc_type,
        hardy_bpv7::crc::CrcType::CRC32_CASTAGNOLI,
        "the payload keeps its CRC through signing, encryption and decryption"
    );
}

#[test]
fn test_bib_removal_and_readd() {
    // 1. Create a bundle
    let (_bundle, bundle_bytes) =
        Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_report_to("ipn:2.1".parse().unwrap())
            .with_lifetime(Duration::from_millis(1000))
            .with_payload(b"test payload".as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();

    let sign_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "HS256",
        "key_ops": ["sign", "verify"],
        "k": rand_k(18)
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

    let bib_count = count_blocks_of_type(&parsed_signed, Type::BlockIntegrity);
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
    let bib_count_after = count_blocks_of_type(&parsed_unsigned, Type::BlockIntegrity);
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
            .with_lifetime(Duration::from_millis(1000))
            .with_payload(b"payload data".as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();

    let sign_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "HS256",
        "key_ops": ["sign", "verify"],
        "k": rand_k(18)
    }))
    .unwrap();
    let enc_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "A128KW",
        "enc": "A128GCM",
        "key_ops": ["encrypt", "decrypt", "wrapKey", "unwrapKey"],
        "k": rand_k(16)
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
    // 1. Create and sign bundle. The payload carries no CRC, as RFC 9173
    // §3.8.1 leaves a signed target, so the tamper reaches the signature
    // check rather than the CRC.
    let (_bundle, bundle_bytes) =
        Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_report_to("ipn:2.1".parse().unwrap())
            .with_lifetime(Duration::from_millis(1000))
            .add_extension_block(Type::Payload)
            .unwrap()
            .with_flags(Flags {
                delete_bundle_on_failure: true,
                ..Default::default()
            })
            .with_crc_type(crc::CrcType::None)
            .build(b"original payload".as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();

    let sign_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "HS256",
        "key_ops": ["sign", "verify"],
        "k": rand_k(18)
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
            .with_lifetime(Duration::from_millis(1000))
            .with_payload(b"encrypted data".as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();

    let enc_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "A128KW",
        "enc": "A128GCM",
        "key_ops": ["encrypt", "decrypt", "wrapKey", "unwrapKey"],
        "k": rand_k(16)
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
    let bcb_count = count_blocks_of_type(&parsed_enc, Type::BlockSecurity);
    assert_eq!(bcb_count, 1, "Should have 1 BCB after encryption");

    // 3. Remove BCB using Editor::remove_encryption
    let raw = raw_of(&encrypted_bytes);
    let editor = Editor::new(&raw, &encrypted_bytes)
        .with_key_source(&keys)
        .remove_encryption(1)
        .map_err(|(_, e)| e)
        .expect("Failed to remove BCB");
    let decrypted_bytes = editor
        .rebuild()
        .map(|c| Chunk::flatten(c, &encrypted_bytes))
        .expect("Failed to rebuild after BCB removal");

    let (decrypted_bytes, parsed_decrypted, _bcb_ops, _bib_ops) =
        validate_with_keys(&decrypted_bytes, &keys).expect("Failed to parse decrypted bundle");

    // 4. Assert: 0 BCBs, payload is decrypted
    let bcb_count_after = count_blocks_of_type(&parsed_decrypted, Type::BlockSecurity);
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
            "k": rand_k(16)
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
    let bcb_count = count_blocks_of_type(&raw, Type::BlockSecurity);
    assert_eq!(bcb_count, 0, "Should have 0 BCBs (bundle is not encrypted)");
    let _ = bundle;

    // Attempt to remove encryption from payload block (which is not encrypted)
    let result = Editor::new(&raw, &bundle_bytes)
        .with_key_source(&keys)
        .remove_encryption(1);

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
    let bib_count = count_blocks_of_type(&raw, Type::BlockIntegrity);
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
        "k": rand_k(18)
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
        "k": rand_k(16)
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
        "k": rand_k(18)
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
    let bib_count = count_blocks_of_type(&parsed, Type::BlockIntegrity);
    assert_eq!(
        bib_count, 1,
        "Should have 1 BIB after signing primary block"
    );

    // 5. The primary block keeps its CRC: it is immutable (RFC 9171
    // §4.3.1), so signing it leaves it as it was built.
    let signed_primary = parsed.blocks.get(&0).expect("Primary block missing");
    assert_eq!(
        signed_primary.crc_type, primary.crc_type,
        "signing the primary keeps its CRC"
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
        "k": rand_k(18)
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
fn test_sign_keeps_the_crc_on_the_target_block() {
    // Signing a block leaves its CRC as it stands, type and value: no
    // security operation changes a target's CRC.

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
    let extent = |block: &hardy_bpv7::block::Block| {
        usize::try_from(block.extent.start).unwrap()..usize::try_from(block.extent.end).unwrap()
    };
    let before = &bundle_bytes[extent(payload_block)];

    // 2. Sign the payload block
    let sign_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "HS256",
        "key_ops": ["sign", "verify"],
        "k": rand_k(18)
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

    // 3. Parse the signed bundle: the payload block keeps its CRC type
    let keys = key::KeySet::new(vec![sign_key]);
    let (signed_bytes, parsed, bcb_ops, bib_ops) =
        validate_with_keys(&signed_bytes, &keys).expect("Failed to parse signed bundle");

    let signed_payload = parsed.blocks.get(&1).expect("Payload block missing");
    assert_eq!(
        signed_payload.crc_type, payload_block.crc_type,
        "signing keeps the payload's CRC type"
    );

    // 4. Verify the signature still works
    verify_block(1, &parsed.blocks, &signed_bytes, &bcb_ops, &bib_ops, &keys)
        .expect("Failed to verify signature on payload block");

    // 5. The payload block's bytes are unchanged, CRC field included: a
    // CBOR array of 6 elements (0x86), not 5.
    let payload_cbor = &signed_bytes[extent(signed_payload)];
    assert_eq!(
        payload_cbor, before,
        "the payload block's bytes are unchanged"
    );
    assert_eq!(
        payload_cbor[0], 0x86,
        "Payload block CBOR should be an array of 6 elements (0x86), got 0x{:02x}",
        payload_cbor[0]
    );
}

// The scope flags name exactly RFC 9173 §3.3.3's three bits (shared by
// §4.3.4's AAD scope), each decoding to the field the RFC names it: every
// other bit round-trips as unrecognised, and a named bit carried in
// `unrecognised` encodes as its bit and canonicalizes to its field.
#[test]
fn scope_flags_name_exactly_the_rfc_bits() {
    let named = [
        (
            0,
            ScopeFlags {
                include_primary_block: true,
                ..ScopeFlags::NONE
            },
        ),
        (
            1,
            ScopeFlags {
                include_target_header: true,
                ..ScopeFlags::NONE
            },
        ),
        (
            2,
            ScopeFlags {
                include_security_header: true,
                ..ScopeFlags::NONE
            },
        ),
    ];
    for bit in 0..64 {
        let value = 1u64 << bit;
        let decoded = ScopeFlags::from(value);
        assert!(
            decoded.is_canonical(),
            "bit {bit}: a decoded value is canonical"
        );
        let named_flags = named
            .iter()
            .find(|(named_bit, _)| *named_bit == bit)
            .map(|(_, flags)| flags.clone());
        let expected = named_flags.clone().unwrap_or(ScopeFlags {
            unrecognised: value,
            ..ScopeFlags::NONE
        });
        // Both canonical, so equal encodings mean equal fields.
        assert_eq!(
            decoded, expected,
            "bit {bit}: decodes to the field RFC 9173 names, or as unrecognised"
        );
        assert_eq!(u64::from(&decoded), value, "bit {bit} round-trips");
        let alias = ScopeFlags {
            unrecognised: value,
            ..ScopeFlags::NONE
        };
        assert_eq!(
            u64::from(&alias),
            value,
            "bit {bit}: an alias encodes as its bit"
        );
        assert_eq!(
            alias.is_canonical(),
            named_flags.is_none(),
            "bit {bit}: an alias of a named bit is not canonical"
        );
        let folded = alias.clone().canonicalize();
        assert!(folded.is_canonical(), "bit {bit}: the alias folds");
        assert_eq!(
            folded, decoded,
            "bit {bit}: the alias folds to the decoded value"
        );
        assert_eq!(alias, decoded, "bit {bit}: an alias equals what it encodes");
    }
}

// The primary-block scope bit carried in `unrecognised` on an otherwise
// empty scope: the scope these tests sign and encrypt under. It folds to a
// primary-only scope, which is not the default, so the operation emits it
// as its scope parameter.
fn primary_scope_alias() -> ScopeFlags {
    ScopeFlags {
        unrecognised: 1 << 0,
        ..ScopeFlags::NONE
    }
}

// The alias folded: the scope the emitted parameter must carry.
fn primary_scope() -> ScopeFlags {
    ScopeFlags {
        include_primary_block: true,
        ..ScopeFlags::NONE
    }
}

// Signing canonicalizes the scope: the parameter it emits carries the
// folded, primary-only scope, and the IPPT the source computed agrees with
// it, so the BIB verifies.
#[test]
fn signing_canonicalizes_an_alias_scope_bit() {
    let (_bundle, bundle_bytes) =
        Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_payload(b"aliased scope".as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();
    let sign_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "HS256",
        "key_ops": ["sign", "verify"],
        "k": rand_k(18)
    }))
    .unwrap();
    let keys = key::KeySet::new(vec![sign_key.clone()]);

    let raw = raw_of(&bundle_bytes);
    let signed_bytes = signer::Signer::new(&raw, &bundle_bytes)
        .sign_block(
            1,
            signer::Context::HMAC_SHA2(primary_scope_alias()),
            "ipn:2.1".parse().unwrap(),
            &sign_key,
        )
        .map_err(|(_, e)| e)
        .expect("signing accepts the alias scope")
        .rebuild()
        .expect("the signed bundle rebuilds");

    let (signed_bytes, parsed, bcb_ops, bib_ops) =
        validate_with_keys(&signed_bytes, &keys).expect("the signed bundle verifies at parse");
    // `bib` and `bcb` each name an `Operation`, so both stay module-qualified.
    let bib::Operation::HMAC_SHA2(op) = &bib_ops
        .values()
        .next()
        .expect("the bundle carries one BIB")
        .operations()[&1]
    else {
        panic!("the BIB is BIB-HMAC-SHA2");
    };
    assert_eq!(op.parameters.flags, primary_scope());
    assert!(
        verify_block(1, &parsed.blocks, &signed_bytes, &bcb_ops, &bib_ops, &keys)
            .expect("the BIB verifies under the scope the parameter states"),
        "a BIB covers the payload"
    );
}

// Encryption canonicalizes the scope: the parameter it emits carries the
// folded, primary-only scope, and the AAD the source computed agrees with
// it, so the payload decrypts.
#[test]
fn encryption_canonicalizes_an_alias_scope_bit() {
    let plaintext = b"aliased scope";
    let (_bundle, bundle_bytes) =
        Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_payload(plaintext.as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();
    let enc_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "A128KW",
        "enc": "A128GCM",
        "key_ops": ["encrypt", "decrypt", "wrapKey", "unwrapKey"],
        "k": rand_k(16)
    }))
    .unwrap();
    let keys = key::KeySet::new(vec![enc_key.clone()]);

    let raw = raw_of(&bundle_bytes);
    let encrypted_bytes = encryptor::Encryptor::new(&raw, &bundle_bytes)
        .encrypt_block(
            1,
            encryptor::Context::AES_GCM(primary_scope_alias()),
            "ipn:2.1".parse().unwrap(),
            &enc_key,
        )
        .map_err(|(_, e)| e)
        .expect("encryption accepts the alias scope")
        .rebuild()
        .expect("the encrypted bundle rebuilds");

    let (encrypted_bytes, parsed, bcb_ops, _bib_ops) =
        validate_with_keys(&encrypted_bytes, &keys).expect("the encrypted bundle parses");
    let bcb::Operation::AES_GCM(op) = &bcb_ops
        .values()
        .next()
        .expect("the bundle carries one BCB")
        .operations()[&1]
    else {
        panic!("the BCB is BCB-AES-GCM");
    };
    assert_eq!(op.parameters.flags, primary_scope());
    let decrypted = block_data(1, &parsed.blocks, &encrypted_bytes, &bcb_ops, &keys)
        .expect("the payload decrypts under the scope the parameter states");
    assert_eq!(decrypted.as_ref(), plaintext);
}

// Scope equality compares the encoding, so an aliased scope and its
// canonical form are one security context: signing two blocks under them
// yields one BIB carrying both targets, not two BIBs with identical
// parameters.
#[test]
fn aliased_and_canonical_scopes_share_one_bib() {
    let (_bundle, bundle_bytes) =
        Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .add_extension_block(Type::Unrecognised(200))
            .unwrap()
            .build(b"ext-data".as_slice().into())
            .with_payload(b"aliased scope".as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();
    let sign_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "HS256",
        "key_ops": ["sign", "verify"],
        "k": rand_k(18)
    }))
    .unwrap();
    let keys = key::KeySet::new(vec![sign_key.clone()]);

    let raw = raw_of(&bundle_bytes);
    let ext = *raw
        .blocks
        .iter()
        .find(|(_, b)| b.block_type == Type::Unrecognised(200))
        .expect("the extension block is present")
        .0;
    let signed_bytes = signer::Signer::new(&raw, &bundle_bytes)
        .sign_block(
            1,
            signer::Context::HMAC_SHA2(primary_scope_alias()),
            "ipn:2.1".parse().unwrap(),
            &sign_key,
        )
        .map_err(|(_, e)| e)
        .expect("signing accepts the alias scope")
        .sign_block(
            ext,
            signer::Context::HMAC_SHA2(primary_scope()),
            "ipn:2.1".parse().unwrap(),
            &sign_key,
        )
        .map_err(|(_, e)| e)
        .expect("signing accepts the canonical scope")
        .rebuild()
        .expect("the signed bundle rebuilds");

    let (_, _, _, bib_ops) =
        validate_with_keys(&signed_bytes, &keys).expect("the signed bundle verifies at parse");
    assert_eq!(bib_ops.len(), 1, "one BIB for the one scope");
    assert_eq!(bib_ops.values().next().unwrap().operations().len(), 2);
}

// A bare HS256 key with a generated value.
fn hs256_key() -> key::Key {
    serde_json::from_value(serde_json::json!({
        "kty": "oct",
        "alg": "HS256",
        "key_ops": ["sign", "verify"],
        "k": rand_k(32)
    }))
    .unwrap()
}

// One session, two sources: one signs the primary, which carries a CRC,
// the other the payload under the default scope, whose IPPT includes the
// primary. The primary keeps its CRC, so both verify in whichever order the
// groups are built; the session repeats so that both orders occur with near
// certainty (64 draws of the group order).
#[test]
fn two_groups_signing_a_crc_primary_verify_in_either_order() {
    let (_, bundle_bytes) = Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
        .with_payload(b"two groups".as_slice().into())
        .build(CreationTimestamp::now())
        .unwrap();
    let raw = raw_of(&bundle_bytes);
    assert!(
        !matches!(raw.primary.crc_type, hardy_bpv7::crc::CrcType::None),
        "precondition: the primary carries a CRC"
    );
    let sign_key = hs256_key();
    let keys = key::KeySet::new(vec![sign_key.clone()]);

    for _ in 0..64 {
        let signed_bytes = signer::Signer::new(&raw, &bundle_bytes)
            .sign_block(
                0,
                signer::Context::HMAC_SHA2(ScopeFlags::default()),
                "ipn:2.1".parse().unwrap(),
                &sign_key,
            )
            .map_err(|(_, e)| e)
            .expect("sign the primary")
            .sign_block(
                1,
                signer::Context::HMAC_SHA2(ScopeFlags::default()),
                "ipn:3.1".parse().unwrap(),
                &sign_key,
            )
            .map_err(|(_, e)| e)
            .expect("sign the payload")
            .rebuild()
            .expect("rebuild the signed bundle");
        let (signed_bytes, parsed, bcb_ops, bib_ops) =
            validate_with_keys(&signed_bytes, &keys).expect("the signed bundle validates");
        assert_eq!(count_blocks_of_type(&parsed, Type::BlockIntegrity), 2);
        for target in [0, 1] {
            verify_block(
                target,
                &parsed.blocks,
                &signed_bytes,
                &bcb_ops,
                &bib_ops,
                &keys,
            )
            .expect("each group's signature verifies");
        }
    }
}

// Signing a primary that carries a CRC leaves the primary unchanged (RFC
// 9171 §4.3.1), so an operation already in the bundle whose scope includes
// the primary still verifies beside the new one, whatever that scope.
#[test]
fn signing_a_crc_primary_keeps_a_primary_scoped_operation_valid() {
    let (_, bundle_bytes) = Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
        .with_payload(b"later primary".as_slice().into())
        .build(CreationTimestamp::now())
        .unwrap();
    let raw = raw_of(&bundle_bytes);
    let sign_key = hs256_key();
    let sign_payload = |scope: ScopeFlags| {
        let bytes = signer::Signer::new(&raw, &bundle_bytes)
            .sign_block(
                1,
                signer::Context::HMAC_SHA2(scope),
                "ipn:3.1".parse().unwrap(),
                &sign_key,
            )
            .map_err(|(_, e)| e)
            .expect("sign the payload")
            .rebuild()
            .expect("rebuild the signed bundle");
        (raw_of(&bytes), bytes)
    };
    let sign_primary = |bundle: &Bundle, bytes: &[u8]| {
        signer::Signer::new(bundle, bytes)
            .sign_block(
                0,
                signer::Context::HMAC_SHA2(ScopeFlags::default()),
                "ipn:2.1".parse().unwrap(),
                &sign_key,
            )
            .map(|signer| signer.rebuild().expect("rebuild the signed bundle"))
            .map_err(|(_, e)| e)
    };

    let keys = key::KeySet::new(vec![sign_key.clone()]);
    for include_primary_block in [true, false] {
        let (signed, signed_bytes) = sign_payload(ScopeFlags {
            include_primary_block,
            ..ScopeFlags::default()
        });
        let both = sign_primary(&signed, &signed_bytes).expect("the primary is signed");
        let (both, parsed, bcb_ops, bib_ops) =
            validate_with_keys(&both, &keys).expect("the signed bundle validates");
        assert_eq!(parsed.primary.crc_type, raw.primary.crc_type);
        for target in [0, 1] {
            verify_block(target, &parsed.blocks, &both, &bcb_ops, &bib_ops, &keys).unwrap_or_else(
                |e| {
                    panic!(
                        "block {target} verifies (primary in scope: {include_primary_block}): {e}"
                    )
                },
            );
        }
    }
}

// A target already queued in a session is refused, rather than its first
// request silently replaced.
#[test]
fn signing_a_target_twice_in_one_session_is_refused() {
    let (_, bundle_bytes) = Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
        .with_payload(b"twice".as_slice().into())
        .build(CreationTimestamp::now())
        .unwrap();
    let raw = raw_of(&bundle_bytes);
    let sign_key = hs256_key();
    let result = signer::Signer::new(&raw, &bundle_bytes)
        .sign_block(
            1,
            signer::Context::HMAC_SHA2(ScopeFlags::default()),
            "ipn:2.1".parse().unwrap(),
            &sign_key,
        )
        .map_err(|(_, e)| e)
        .expect("sign the payload")
        .sign_block(
            1,
            signer::Context::HMAC_SHA2(ScopeFlags::default()),
            "ipn:3.1".parse().unwrap(),
            &sign_key,
        );
    assert!(matches!(result, Err((_, signer::Error::AlreadySigned(1)))));
}

// A BIB carries one parameter set, so under key wrap one CEK, wrapped once,
// keys every target (RFC 9173 §3.8.2): each target of a key-wrapped
// two-target BIB verifies. The scope leaves the security header out, so the
// targets can share the BIB.
#[test]
fn a_key_wrapped_multi_target_bib_verifies_every_target() {
    let (_, bundle_bytes) = Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
        .add_extension_block(Type::Unrecognised(200))
        .unwrap()
        .build(b"ext-data".as_slice().into())
        .with_payload(b"wrapped".as_slice().into())
        .build(CreationTimestamp::now())
        .unwrap();
    let raw = raw_of(&bundle_bytes);
    let ext = *raw
        .blocks
        .iter()
        .find(|(_, b)| b.block_type == Type::Unrecognised(200))
        .expect("the extension block is present")
        .0;
    let kek: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "HS256+A128KW",
        "key_ops": ["sign", "verify", "wrapKey", "unwrapKey"],
        "k": rand_k(16)
    }))
    .unwrap();

    let signed_bytes = signer::Signer::new(&raw, &bundle_bytes)
        .sign_block(
            1,
            signer::Context::HMAC_SHA2(shareable_scope()),
            "ipn:2.1".parse().unwrap(),
            &kek,
        )
        .map_err(|(_, e)| e)
        .expect("sign the payload")
        .sign_block(
            ext,
            signer::Context::HMAC_SHA2(shareable_scope()),
            "ipn:2.1".parse().unwrap(),
            &kek,
        )
        .map_err(|(_, e)| e)
        .expect("sign the extension block")
        .rebuild()
        .expect("rebuild the signed bundle");
    let keys = key::KeySet::new(vec![kek]);
    let (signed_bytes, parsed, bcb_ops, bib_ops) =
        validate_with_keys(&signed_bytes, &keys).expect("the signed bundle validates");
    assert_eq!(count_blocks_of_type(&parsed, Type::BlockIntegrity), 1);
    for target in [1, ext] {
        verify_block(
            target,
            &parsed.blocks,
            &signed_bytes,
            &bcb_ops,
            &bib_ops,
            &keys,
        )
        .expect("each target verifies under the one wrapped key");
    }
}

// A scope under which targets can share one BIB: the default without the
// security header (RFC 9172 erratum 8723).
fn shareable_scope() -> ScopeFlags {
    ScopeFlags {
        include_security_header: false,
        ..ScopeFlags::default()
    }
}

// A bundle whose payload and extension block are signed by one source,
// with `first` and `second` keys under `scope`: (bytes, extension block
// number).
fn sign_payload_and_extension(
    scope: ScopeFlags,
    first: &key::Key,
    second: &key::Key,
) -> (Box<[u8]>, u64) {
    let (_, bundle_bytes) = Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
        .add_extension_block(Type::Unrecognised(200))
        .unwrap()
        .build(b"ext-data".as_slice().into())
        .with_payload(b"two targets".as_slice().into())
        .build(CreationTimestamp::now())
        .unwrap();
    let raw = raw_of(&bundle_bytes);
    let ext = *raw
        .blocks
        .iter()
        .find(|(_, b)| b.block_type == Type::Unrecognised(200))
        .expect("the extension block is present")
        .0;
    let signed_bytes = signer::Signer::new(&raw, &bundle_bytes)
        .sign_block(
            1,
            signer::Context::HMAC_SHA2(scope.clone()),
            "ipn:2.1".parse().unwrap(),
            first,
        )
        .map_err(|(_, e)| e)
        .expect("sign the payload")
        .sign_block(
            ext,
            signer::Context::HMAC_SHA2(scope),
            "ipn:2.1".parse().unwrap(),
            second,
        )
        .map_err(|(_, e)| e)
        .expect("sign the extension block")
        .rebuild()
        .expect("rebuild the signed bundle");
    (signed_bytes, ext)
}

// Under a scope that includes the security header, BIB-HMAC-SHA2 cannot
// share a BIB (RFC 9172 erratum 8723): two targets from one source and key
// get a BIB each, and each verifies.
#[test]
fn a_security_header_scope_signs_each_target_alone() {
    let key = hs256_key();
    let (signed_bytes, ext) = sign_payload_and_extension(ScopeFlags::default(), &key, &key);
    let keys = key::KeySet::new(vec![key]);
    let (signed_bytes, parsed, bcb_ops, bib_ops) =
        validate_with_keys(&signed_bytes, &keys).expect("the signed bundle validates");
    assert_eq!(count_blocks_of_type(&parsed, Type::BlockIntegrity), 2);
    for target in [1, ext] {
        verify_block(
            target,
            &parsed.blocks,
            &signed_bytes,
            &bcb_ops,
            &bib_ops,
            &keys,
        )
        .expect("each target verifies");
    }
}

// Operations under different keys are bound for different security
// acceptors, so they never share a BIB, even where the context allows it;
// each verifies under its own key.
#[test]
fn targets_under_different_keys_get_separate_bibs() {
    let (first, second) = (hs256_key(), hs256_key());
    let (signed_bytes, ext) = sign_payload_and_extension(shareable_scope(), &first, &second);
    let no_keys = key::KeySet::new(Vec::new());
    let (signed_bytes, parsed, bcb_ops, bib_ops) =
        validate_with_keys(&signed_bytes, &no_keys).expect("the signed bundle parses");
    assert_eq!(count_blocks_of_type(&parsed, Type::BlockIntegrity), 2);
    for (target, key) in [(1, first), (ext, second)] {
        let keys = key::KeySet::new(vec![key]);
        assert!(
            verify_block(
                target,
                &parsed.blocks,
                &signed_bytes,
                &bcb_ops,
                &bib_ops,
                &keys,
            )
            .expect("each target verifies under its own key"),
            "target {target} carries a BIB"
        );
    }
}

// Encrypting the Bundle Age block of the RFC 9173 A.3 bundle would encrypt
// the BIB over it with every block that BIB covers (RFC 9172 §3.9), the
// primary among them, which no BCB can target: refused, naming the BIB.
#[test]
fn encrypting_under_a_bib_that_covers_the_primary_is_refused() {
    let raw = raw_of(&APPENDIX_A_3);
    let BibCoverage::Some(bib) = raw.blocks[&2].bib else {
        panic!("the Bundle Age block is signed");
    };
    assert!(
        matches!(raw.blocks[&0].bib, BibCoverage::Some(n) if n == bib),
        "precondition: one BIB covers the primary and the Bundle Age block"
    );
    let enc_key: key::Key = serde_json::from_value(serde_json::json!({
        "kty": "oct",
        "alg": "dir",
        "enc": "A128GCM",
        "key_ops": ["encrypt", "decrypt"],
        "k": rand_k(16)
    }))
    .unwrap();
    let result = encryptor::Encryptor::new(&raw, &APPENDIX_A_3).encrypt_block(
        2,
        encryptor::Context::AES_GCM(ScopeFlags::default()),
        "ipn:2.1".parse().unwrap(),
        &enc_key,
    );
    assert!(matches!(
        result,
        Err((_, encryptor::Error::BibCoversPrimary(n))) if n == bib
    ));
}

// A bundle whose payload is encrypted, and first signed when `sign` is set
// (the BIB is then encrypted under a BCB of its own, RFC 9172 §3.9), with
// the keys that made it: the signing key first when there is one.
fn encrypted_payload(sign: bool) -> (Box<[u8]>, key::KeySet) {
    let (_, bundle_bytes) = Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
        .with_report_to("ipn:2.1".parse().unwrap())
        .with_lifetime(Duration::from_millis(1000))
        .with_payload(b"accepted at ingress".as_slice().into())
        .build(CreationTimestamp::now())
        .unwrap();
    let enc_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "A128KW",
        "enc": "A128GCM",
        "key_ops": ["encrypt", "decrypt", "wrapKey", "unwrapKey"],
        "k": rand_k(16)
    }))
    .unwrap();
    let mut keys = Vec::new();

    let mut bytes = bundle_bytes;
    if sign {
        let sign_key: key::Key = serde_json::from_value(serde_json::json!({
            "kid": "ipn:2.1",
            "kty": "oct",
            "alg": "HS256",
            "key_ops": ["sign", "verify"],
            "k": rand_k(18)
        }))
        .unwrap();
        let raw = raw_of(&bytes);
        bytes = signer::Signer::new(&raw, &bytes)
            .sign_block(
                1,
                signer::Context::HMAC_SHA2(ScopeFlags::default()),
                "ipn:2.1".parse().unwrap(),
                &sign_key,
            )
            .map_err(|(_, e)| e)
            .unwrap()
            .rebuild()
            .unwrap();
        keys.push(sign_key);
    }

    let raw = raw_of(&bytes);
    let encrypted = encryptor::Encryptor::new(&raw, &bytes)
        .encrypt_block(
            1,
            encryptor::Context::AES_GCM(ScopeFlags {
                include_security_header: false,
                ..ScopeFlags::default()
            }),
            "ipn:2.1".parse().unwrap(),
            &enc_key,
        )
        .map_err(|(_, e)| e)
        .unwrap()
        .rebuild()
        .unwrap();
    keys.push(enc_key);
    (encrypted, key::KeySet::new(keys))
}

// The plaintext of each block in `numbers`, as an acceptor's earlier
// decrypt holds it.
fn plaintexts_of(
    numbers: &[u64],
    blocks: &HashMap<u64, Block>,
    data: &[u8],
    bcb_ops: &HashMap<u64, bpsec::bcb::OperationSet>,
    keys: &key::KeySet,
) -> Decrypted {
    let plaintext = numbers
        .iter()
        .map(|&n| {
            let plaintext = block_data(n, blocks, data, bcb_ops, keys).expect("the block decrypts");
            (n, Zeroizing::new(Box::from(plaintext.as_ref())))
        })
        .collect();
    Decrypted {
        plaintext,
        ..Default::default()
    }
}

/// Removing a block's encryption from plaintext an acceptor already holds
/// writes the same bundle as decrypting it again.
#[test]
fn removing_encryption_from_supplied_plaintext_matches_decrypting() {
    let (bytes, keys) = encrypted_payload(false);
    let (bytes, parsed, bcb_ops, _) = validate_with_keys(&bytes, &keys).unwrap();
    let plaintexts = plaintexts_of(&[1], &parsed.blocks, &bytes, &bcb_ops, &keys);

    let decrypted = Editor::new(&parsed, &bytes)
        .with_key_source(&keys)
        .remove_encryption(1)
        .map_err(|(_, e)| e)
        .unwrap()
        .rebuild()
        .map(|c| Chunk::flatten(c, &bytes))
        .unwrap();
    // No keys: the supplied plaintext is all the removal reads.
    let supplied = Editor::new(&parsed, &bytes)
        .with_decrypted(&plaintexts)
        .remove_encryption(1)
        .map_err(|(_, e)| e)
        .unwrap()
        .rebuild()
        .map(|c| Chunk::flatten(c, &bytes))
        .unwrap();
    assert_eq!(supplied, decrypted);

    let after = raw_of(&supplied);
    assert_eq!(count_blocks_of_type(&after, Type::BlockSecurity), 0);
    assert_eq!(
        after.blocks[&1].payload(&supplied).unwrap(),
        b"accepted at ingress"
    );
}

/// An acceptor of the BCB over a payload an encrypted BIB covers removes
/// the BCB over the BIB with it, from plaintext it holds: the BIB is
/// written in plaintext with its operation intact, and stripping the
/// accepted operation removes it.
#[test]
fn accepting_an_encrypted_bib_and_its_target_from_plaintext_strips_both() {
    let (bytes, keys) = encrypted_payload(true);
    let (bytes, parsed, bcb_ops, _) = validate_with_keys(&bytes, &keys).unwrap();
    let bib = parsed
        .blocks
        .iter()
        .find_map(|(&n, b)| (b.block_type == Type::BlockIntegrity).then_some(n))
        .expect("the BIB is present");
    assert_eq!(parsed.blocks[&1].bib, BibCoverage::Maybe);
    assert_eq!(count_blocks_of_type(&parsed, Type::BlockSecurity), 2);
    let plaintexts = plaintexts_of(&[1, bib], &parsed.blocks, &bytes, &bcb_ops, &keys);

    // No keys: the supplied plaintext is all the removal reads.
    let editor = Editor::new(&parsed, &bytes)
        .with_decrypted(&plaintexts)
        .remove_encryption(1)
        .map_err(|(_, e)| e)
        .unwrap();
    let accepted = editor
        .remove_integrity(1)
        .map_err(|(_, e)| e)
        .unwrap()
        .rebuild()
        .map(|c| Chunk::flatten(c, &bytes))
        .unwrap();

    let after = raw_of(&accepted);
    assert_eq!(count_blocks_of_type(&after, Type::BlockSecurity), 0);
    assert_eq!(count_blocks_of_type(&after, Type::BlockIntegrity), 0);
    assert_eq!(
        after.blocks[&1].payload(&accepted).unwrap(),
        b"accepted at ingress"
    );
}
