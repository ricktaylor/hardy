use hardy_bpv7::{
    block,
    bpsec::{key, rfc9173::ScopeFlags, signer},
    builder::Builder,
    bundle,
    creation_timestamp::CreationTimestamp,
    editor::{self, Editor},
};

// R-3: Editor::remove_block must reject a security block (BIB/BCB). Removing a
// BCB directly would leave its targets holding ciphertext with no covering BCB
// (ciphertext surfaced as plaintext on reparse); removing a BIB would silently
// strip integrity. Security blocks are managed only via Signer/Encryptor
// (remove_integrity/remove_encryption), as push/insert/update_block also enforce.
#[test]
fn remove_block_rejects_security_block() {
    let (bundle, bundle_bytes) =
        Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_payload(b"remove-bib test".as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();

    let kek: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "HS256+A128KW",
        "key_ops": ["sign", "verify", "wrapKey", "unwrapKey"],
        "k": "AAAAAAAAAAAAAAAAAAAAAA"
    }))
    .unwrap();
    let keys = key::KeySet::new(vec![kek.clone()]);

    let signed_bytes = signer::Signer::new(&bundle, &bundle_bytes)
        .sign_block(
            1,
            signer::Context::HMAC_SHA2(ScopeFlags::default()),
            "ipn:2.1".parse().unwrap(),
            &kek,
        )
        .map_err(|(_, e)| e)
        .expect("Failed to sign")
        .rebuild()
        .expect("Failed to rebuild");

    let parsed = bundle::ParsedBundle::parse_with_keys(&signed_bytes, &keys)
        .expect("Failed to parse signed bundle");

    let bib_num = parsed
        .bundle
        .blocks
        .iter()
        .find(|(_, b)| matches!(b.block_type, block::Type::BlockIntegrity))
        .map(|(n, _)| *n)
        .expect("Signed bundle should contain a BIB block");

    let result = Editor::new(&parsed.bundle, &signed_bytes).remove_block(bib_num);
    assert!(
        matches!(result, Err((_, editor::Error::SecurityBlock))),
        "remove_block must reject a BIB block with Error::SecurityBlock"
    );
}
