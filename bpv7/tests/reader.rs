//! Public-API tests for `bpsec::DecryptingReader`: the four `Availability`
//! states, the lend (trait) / give (inherent) split, and the memoisation of
//! decrypt outcomes.

use core::cell::Cell;

use hardy_bpv7::{
    block::Payload,
    bpsec::{self, DecryptingReader, encryptor, key, rfc9173::ScopeFlags},
    builder::Builder,
    creation_timestamp::CreationTimestamp,
    eid::Eid,
    parse,
    reader::{Availability, Reader, ReaderExt},
};
use std::collections::HashMap;

mod common;
use self::common::rand_k;

const PAYLOAD: &[u8] = b"reader memoisation plaintext";

/// A key source that counts lookups, for asserting decrypt attempts
/// deterministically: a memoised outcome must not consult the keys again.
struct CountingKeys<'a> {
    inner: &'a key::KeySet,
    hits: Cell<usize>,
}

impl<'a> CountingKeys<'a> {
    fn new(inner: &'a key::KeySet) -> Self {
        Self {
            inner,
            hits: Cell::new(0),
        }
    }

    fn hits(&self) -> usize {
        self.hits.get()
    }
}

impl key::KeySource for CountingKeys<'_> {
    fn key<'k>(&'k self, source: &Eid, operations: &[key::Operation]) -> Option<&'k key::Key> {
        self.hits.set(self.hits.get() + 1);
        self.inner.key(source, operations)
    }
}

fn enc_key() -> key::Key {
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

/// A parsed bundle whose payload block (1) is BCB-encrypted with `key`:
/// the bundle bytes, the blocks, and the decoded BCB OperationSets.
#[allow(clippy::type_complexity)]
fn encrypted_bundle(
    key: &key::Key,
) -> (
    bytes::Bytes,
    HashMap<u64, hardy_bpv7::block::Block>,
    HashMap<u64, bpsec::bcb::OperationSet>,
) {
    let (_, bundle_bytes) = Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
        .with_payload(PAYLOAD.into())
        .build(CreationTimestamp::now())
        .unwrap();

    let raw = parse::parse(bytes::Bytes::copy_from_slice(&bundle_bytes))
        .expect("parse")
        .bundle;
    // Exclude the security header from AAD to avoid mismatches due to BCB
    // header mutation.
    let flags = ScopeFlags {
        include_security_header: false,
        ..ScopeFlags::default()
    };
    let encryptor = encryptor::Encryptor::new(&raw, &bundle_bytes)
        .encrypt_block(
            1,
            encryptor::Context::AES_GCM(flags),
            "ipn:2.1".parse().unwrap(),
            key,
        )
        .map_err(|(_, e)| e)
        .expect("encrypt payload block");
    let encrypted_bytes = encryptor.rebuild().expect("rebuild encrypted bundle");

    let parse::Parsed {
        data, bundle, bcbs, ..
    } = parse::parse(bytes::Bytes::copy_from_slice(&encrypted_bytes)).expect("parse encrypted");
    (data, bundle.blocks, bcbs)
}

#[test]
fn covered_block_decrypts_once_and_lends() {
    let key = enc_key();
    let keys = key::KeySet::new(vec![key.clone()]);
    let (data, blocks, bcbs) = encrypted_bundle(&key);

    let counting = CountingKeys::new(&keys);
    let reader = DecryptingReader::new(&blocks, &data, &bcbs, &counting);

    let after_first = {
        let (_, availability) = reader.block(1).expect("payload block exists");
        let Availability::Available(payload) = availability else {
            panic!("payload must decrypt, got {availability:?}");
        };
        assert!(
            matches!(payload, Payload::Borrowed(_)),
            "the trait path lends a borrow of the cache"
        );
        assert_eq!(payload.as_ref(), PAYLOAD);
        counting.hits()
    };
    assert!(after_first >= 1, "the first read must consult the keys");

    for _ in 0..2 {
        let (_, availability) = reader.block(1).expect("payload block exists");
        let Availability::Available(payload) = availability else {
            panic!("repeat reads replay the cached plaintext");
        };
        assert_eq!(payload.as_ref(), PAYLOAD);
    }
    assert_eq!(
        counting.hits(),
        after_first,
        "repeat reads must not consult the keys again"
    );

    // The inherent door gives the same plaintext, owned, from the cache.
    let payload = reader
        .block_data(1)
        .expect("cached plaintext")
        .expect("resident");
    assert!(
        matches!(payload, Payload::Decrypted(_)),
        "the inherent path gives owned plaintext, never a cache borrow"
    );
    assert_eq!(payload.as_ref(), PAYLOAD);
    assert_eq!(
        counting.hits(),
        after_first,
        "the inherent path reuses the cached plaintext"
    );
}

#[test]
fn uncovered_block_borrows_the_wire() {
    let (_, bundle_bytes) = Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
        .with_payload(PAYLOAD.into())
        .build(CreationTimestamp::now())
        .unwrap();
    let parse::Parsed {
        data, bundle, bcbs, ..
    } = parse::parse(bytes::Bytes::copy_from_slice(&bundle_bytes)).expect("parse");

    let keys = key::KeySet::EMPTY;
    let reader = DecryptingReader::new(&bundle.blocks, &data, &bcbs, &keys);

    let (_, availability) = reader.block(1).expect("payload block exists");
    let Availability::Available(payload) = availability else {
        panic!("an uncovered resident block is available, got {availability:?}");
    };
    assert!(matches!(payload, Payload::Borrowed(_)));
    assert_eq!(payload.as_ref(), PAYLOAD);

    let payload = reader.block_data(1).expect("uncovered").expect("resident");
    assert!(
        matches!(payload, Payload::Borrowed(_)),
        "uncovered plaintext is a wire slice on both doors"
    );
    assert_eq!(payload.as_ref(), PAYLOAD);
}

#[test]
fn no_key_is_a_replayed_state() {
    let key = enc_key();
    let (data, blocks, bcbs) = encrypted_bundle(&key);

    let empty = key::KeySet::EMPTY;
    let counting = CountingKeys::new(&empty);
    let reader = DecryptingReader::new(&blocks, &data, &bcbs, &counting);

    let after_first = {
        assert!(matches!(
            reader.block(1).expect("payload block exists").1,
            Availability::NoKey
        ));
        counting.hits()
    };
    for _ in 0..2 {
        assert!(matches!(
            reader.block(1).expect("payload block exists").1,
            Availability::NoKey
        ));
    }
    assert_eq!(
        counting.hits(),
        after_first,
        "a no-key outcome is attempted once and replayed"
    );

    // Inherent parity: the same state as a typed error, also from the cache.
    assert!(matches!(
        reader.block_data(1),
        Err(hardy_bpv7::Error::InvalidBPSec(bpsec::Error::NoKey))
    ));
    assert_eq!(
        counting.hits(),
        after_first,
        "the cached no-key replays without a key lookup"
    );
}

#[test]
fn non_resident_extent_is_not_resident() {
    let key = enc_key();
    let keys = key::KeySet::new(vec![key.clone()]);
    let (data, blocks, bcbs) = encrypted_bundle(&key);

    // Cut into the payload block's extent: the blocks were parsed from the
    // full buffer, the reader sees a truncated one (the headers-only or
    // streaming case).
    let truncated = &data[..data.len() - 2];
    assert!(
        blocks.get(&1).unwrap().payload_range().end > truncated.len() as u64,
        "the truncation must cut the payload extent"
    );

    let reader = DecryptingReader::new(&blocks, truncated, &bcbs, &keys);
    assert!(matches!(
        reader.block(1).expect("payload block exists").1,
        Availability::NotResident
    ));
    assert!(
        reader.block_data(1).expect("not an error").is_none(),
        "the inherent door reports non-residency as Ok(None)"
    );
}

#[test]
fn corrupted_ciphertext_is_not_decryptable() {
    let key = enc_key();
    let keys = key::KeySet::new(vec![key.clone()]);
    let (data, blocks, bcbs) = encrypted_bundle(&key);

    // Flip one ciphertext byte inside the covered payload's extent; the
    // blocks map still indexes the same offsets.
    let mut corrupt = data.to_vec();
    let start = usize::try_from(blocks.get(&1).unwrap().payload_range().start).unwrap();
    corrupt[start] ^= 0x01;

    let counting = CountingKeys::new(&keys);
    let reader = DecryptingReader::new(&blocks, &corrupt, &bcbs, &counting);

    let after_first = {
        assert!(matches!(
            reader.block(1).expect("payload block exists").1,
            Availability::NotDecryptable
        ));
        counting.hits()
    };
    assert!(matches!(
        reader.block(1).expect("payload block exists").1,
        Availability::NotDecryptable
    ));
    assert_eq!(
        counting.hits(),
        after_first,
        "a failed decrypt is attempted once and replayed"
    );

    // Inherent parity: re-runs the decrypt to surface the exact cause.
    assert!(matches!(
        reader.block_data(1),
        Err(hardy_bpv7::Error::InvalidBPSec(
            bpsec::Error::DecryptionFailed
        ))
    ));
    assert!(
        counting.hits() > after_first,
        "the give door re-runs a cached failure for its cause"
    );
}

// The coverage index says block 1 is BCB-covered, but the OperationSet map
// has no entry — mismatched parse products, a logic error.
#[test]
#[should_panic(expected = "no operation for it")]
fn mismatched_parse_products_panic_on_the_trait_door() {
    let key = enc_key();
    let keys = key::KeySet::new(vec![key.clone()]);
    let (data, blocks, _) = encrypted_bundle(&key);

    let no_ops = HashMap::new();
    let reader = DecryptingReader::new(&blocks, &data, &no_ops, &keys);
    let _ = reader.block(1);
}

#[test]
fn mismatched_parse_products_error_on_the_inherent_door() {
    let key = enc_key();
    let keys = key::KeySet::new(vec![key.clone()]);
    let (data, blocks, _) = encrypted_bundle(&key);

    let no_ops = HashMap::new();
    let reader = DecryptingReader::new(&blocks, &data, &no_ops, &keys);
    assert!(matches!(
        reader.block_data(1),
        Err(hardy_bpv7::Error::Altered)
    ));
}

#[test]
fn extract_decodes_an_available_payload() {
    // A payload that is itself canonical CBOR: extract() decodes it.
    let (_, bundle_bytes) = Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
        .with_payload(hardy_cbor::encode::emit(&42u64).0.into())
        .build(CreationTimestamp::now())
        .unwrap();
    let parse::Parsed {
        data, bundle, bcbs, ..
    } = parse::parse(bytes::Bytes::copy_from_slice(&bundle_bytes)).expect("parse");

    let keys = key::KeySet::EMPTY;
    let reader = DecryptingReader::new(&bundle.blocks, &data, &bcbs, &keys);

    // Through the concrete reader, and through the dyn object the hooks
    // hand out — the blanket impl must cover both.
    assert_eq!(reader.extract::<u64>(1).expect("decodes"), Some(42));
    let dyn_reader: &dyn Reader = &reader;
    assert_eq!(dyn_reader.extract::<u64>(1).expect("decodes"), Some(42));

    // Absent block: None, not an error.
    assert_eq!(reader.extract::<u64>(99).expect("absent is None"), None);
}

#[test]
fn extract_flattens_unavailable_and_reports_decode_failures() {
    let key = enc_key();
    let (data, blocks, bcbs) = encrypted_bundle(&key);

    // Covered with no key: unavailable flattens to None.
    let empty = key::KeySet::EMPTY;
    let reader = DecryptingReader::new(&blocks, &data, &bcbs, &empty);
    assert_eq!(reader.extract::<u64>(1).expect("unavailable is None"), None);

    // Decryptable, but the plaintext is not a CBOR u64: a decode error.
    let keys = key::KeySet::new(vec![key.clone()]);
    let reader = DecryptingReader::new(&blocks, &data, &bcbs, &keys);
    assert!(
        reader
            .extract::<u64>(1)
            .is_err_and(|e| matches!(e, hardy_bpv7::Error::InvalidCBOR(_)))
    );
}
