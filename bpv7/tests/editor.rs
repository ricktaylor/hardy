//! Integration tests for `hardy_bpv7::editor::Editor` — building, mutating,
//! and rebuilding bundles through the public API.

use hardy_bpv7::{
    Bundle, block,
    bpsec::{key, rfc9173::ScopeFlags, signer},
    builder, crc, creation_timestamp,
    editor::{Chunk, Editor, Error},
    eid, hop_info, parse,
};
use std::collections::HashSet;

// Build a bundle, parse it, return (bundle, data) ready for editing.
fn make_bundle() -> (Bundle, Box<[u8]>) {
    let (_, data) = builder::Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
        .with_report_to("ipn:3.0".parse().unwrap())
        .with_payload("Hello".as_bytes().into())
        .build(creation_timestamp::CreationTimestamp::now())
        .unwrap();
    let bundle = reparse(&data);
    (bundle, data)
}

// Build a bundle with a hop count block, then re-parse so block keys match wire numbers.
fn make_bundle_with_hop_count() -> (Bundle, Box<[u8]>) {
    let (_, data) = builder::Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
        .with_hop_count(&hop_info::HopInfo {
            limit: 30,
            count: 0,
        })
        .with_payload("Hello".as_bytes().into())
        .build(creation_timestamp::CreationTimestamp::now())
        .unwrap();
    let bundle = reparse(&data);
    (bundle, data)
}

// Unwrap a Result<T, (Editor, Error)> — panics with the error on failure.
fn ok<T>(result: Result<T, (Editor, Error)>) -> T {
    result.unwrap_or_else(|(_, e)| panic!("Editor operation failed: {e}"))
}

// Edit a bundle, rebuild, re-parse, and return the parsed Bundle.
fn reparse(data: &[u8]) -> Bundle {
    parse::parse(bytes::Bytes::copy_from_slice(data))
        .unwrap()
        .bundle
}

#[test]
fn no_op_rebuild() {
    let (bundle, data) = make_bundle();
    let new_data = Editor::new(&bundle, &data)
        .rebuild()
        .map(|c| Chunk::flatten(c, &data))
        .unwrap();
    let reparsed = reparse(&new_data);
    assert_eq!(reparsed.primary.id.source, bundle.primary.id.source);
    assert_eq!(reparsed.primary.destination, bundle.primary.destination);
}

#[test]
fn change_destination() {
    let (bundle, data) = make_bundle();
    let new_dest: eid::Eid = "ipn:99.0".parse().unwrap();
    let new_data = ok(Editor::new(&bundle, &data).with_destination(new_dest.clone()))
        .rebuild()
        .map(|c| Chunk::flatten(c, &data))
        .unwrap();
    let reparsed = reparse(&new_data);
    assert_eq!(reparsed.primary.destination, new_dest);
    assert_eq!(reparsed.primary.id.source, bundle.primary.id.source);
}

#[test]
fn change_source() {
    let (bundle, data) = make_bundle();
    let new_src: eid::Eid = "ipn:50.0".parse().unwrap();
    let new_data = ok(Editor::new(&bundle, &data).with_source(new_src.clone()))
        .rebuild()
        .map(|c| Chunk::flatten(c, &data))
        .unwrap();
    let reparsed = reparse(&new_data);
    assert_eq!(reparsed.primary.id.source, new_src);
}

#[test]
fn change_report_to() {
    let (bundle, data) = make_bundle();
    let new_rt: eid::Eid = "ipn:77.0".parse().unwrap();
    let new_data = ok(Editor::new(&bundle, &data).with_report_to(new_rt.clone()))
        .rebuild()
        .map(|c| Chunk::flatten(c, &data))
        .unwrap();
    let reparsed = reparse(&new_data);
    assert_eq!(reparsed.primary.report_to, new_rt);
}

#[test]
fn change_lifetime() {
    let (bundle, data) = make_bundle();
    let new_lifetime = core::time::Duration::from_secs(7200);
    let new_data = ok(Editor::new(&bundle, &data).with_lifetime(new_lifetime))
        .rebuild()
        .map(|c| Chunk::flatten(c, &data))
        .unwrap();
    let reparsed = reparse(&new_data);
    assert_eq!(reparsed.primary.lifetime, new_lifetime);
}

#[test]
fn change_crc_type() {
    let (bundle, data) = make_bundle();
    let new_data = ok(Editor::new(&bundle, &data).with_bundle_crc_type(crc::CrcType::CRC16_X25))
        .rebuild()
        .map(|c| Chunk::flatten(c, &data))
        .unwrap();
    let reparsed = reparse(&new_data);
    assert!(matches!(reparsed.primary.crc_type, crc::CrcType::CRC16_X25));
}

#[test]
fn add_extension_block() {
    let (bundle, data) = make_bundle();
    let new_data = ok(Editor::new(&bundle, &data).push_block(block::Type::Unrecognised(200)))
        .with_data((&[0xCA, 0xFE][..]).into())
        .rebuild()
        .rebuild()
        .map(|c| Chunk::flatten(c, &data))
        .unwrap();
    let reparsed = reparse(&new_data);
    assert!(reparsed.blocks.contains_key(&2));
}

#[test]
fn remove_extension_block() {
    let (bundle, data) = make_bundle_with_hop_count();
    let hop_block = bundle
        .blocks
        .iter()
        .find(|(_, b)| matches!(b.block_type, block::Type::HopCount))
        .map(|(n, _)| *n)
        .expect("Should have hop count block");

    let new_data = ok(Editor::new(&bundle, &data).remove_block(hop_block))
        .rebuild()
        .map(|c| Chunk::flatten(c, &data))
        .unwrap();
    let reparsed = reparse(&new_data);
    // Verify the HopCount block was removed (extension-field
    // interpretation is a hardy-bpa concern now — check block types).
    assert!(
        !reparsed
            .blocks
            .values()
            .any(|b| matches!(b.block_type, block::Type::HopCount))
    );
}

#[test]
fn cannot_remove_payload() {
    let (bundle, data) = make_bundle();
    let result = Editor::new(&bundle, &data).remove_block(1);
    assert!(matches!(result, Err((_, Error::PayloadBlock))));
}

#[test]
fn cannot_remove_primary() {
    let (bundle, data) = make_bundle();
    let result = Editor::new(&bundle, &data).remove_block(0);
    assert!(matches!(result, Err((_, Error::PrimaryBlock))));
}

#[test]
fn cannot_add_duplicate_hop_count() {
    let (bundle, data) = make_bundle_with_hop_count();
    let result = Editor::new(&bundle, &data).push_block(block::Type::HopCount);
    assert!(matches!(result, Err((_, Error::IllegalDuplicate(_)))));
}

#[test]
fn multiple_primary_changes() {
    let (bundle, data) = make_bundle();
    let new_dest: eid::Eid = "ipn:99.0".parse().unwrap();
    let new_lifetime = core::time::Duration::from_secs(600);
    let editor = ok(Editor::new(&bundle, &data).with_destination(new_dest.clone()));
    let new_data = ok(editor.with_lifetime(new_lifetime))
        .rebuild()
        .map(|c| Chunk::flatten(c, &data))
        .unwrap();
    let reparsed = reparse(&new_data);
    assert_eq!(reparsed.primary.destination, new_dest);
    assert_eq!(reparsed.primary.lifetime, new_lifetime);
    assert_eq!(reparsed.primary.id.source, bundle.primary.id.source);
}

#[test]
fn insert_new_block_type() {
    let (bundle, data) = make_bundle();
    // insert_block with a new type should add it
    let new_data = ok(Editor::new(&bundle, &data).insert_block(block::Type::Unrecognised(200)))
        .with_data((&[0x01, 0x02][..]).into())
        .rebuild()
        .rebuild()
        .map(|c| Chunk::flatten(c, &data))
        .unwrap();
    let reparsed = reparse(&new_data);
    assert!(reparsed.blocks.contains_key(&2));
}

/// Asserts that the Bundle returned by `rebuild_bundle()` matches a fresh
/// parse of the same data — same block set, same extents, same primary
/// block fields.
fn assert_rebuild_matches_parse(bundle: &Bundle, data: &[u8]) {
    let reparsed = reparse(data);

    // Primary block fields
    assert_eq!(bundle.primary.id.source, reparsed.primary.id.source);
    assert_eq!(bundle.primary.id.timestamp, reparsed.primary.id.timestamp);
    assert_eq!(
        bundle.primary.id.fragment_info,
        reparsed.primary.id.fragment_info
    );
    assert_eq!(bundle.primary.destination, reparsed.primary.destination);
    assert_eq!(bundle.primary.report_to, reparsed.primary.report_to);
    assert_eq!(bundle.primary.lifetime, reparsed.primary.lifetime);
    assert!(
        matches!(
            (&bundle.primary.crc_type, &reparsed.primary.crc_type),
            (crc::CrcType::None, crc::CrcType::None)
                | (crc::CrcType::CRC16_X25, crc::CrcType::CRC16_X25)
                | (
                    crc::CrcType::CRC32_CASTAGNOLI,
                    crc::CrcType::CRC32_CASTAGNOLI
                )
        ),
        "CRC type mismatch"
    );
    assert_eq!(bundle.primary.flags, reparsed.primary.flags);

    // Same set of block numbers
    assert_eq!(
        bundle.blocks.keys().collect::<HashSet<_>>(),
        reparsed.blocks.keys().collect::<HashSet<_>>(),
        "Block sets differ"
    );

    // Block fields match and ranges index validly into the data
    for (block_number, block) in &bundle.blocks {
        let reparsed_block = reparsed.blocks.get(block_number).unwrap();
        assert_eq!(
            block.block_type, reparsed_block.block_type,
            "Block {block_number} type mismatch"
        );
        assert_eq!(
            block.flags, reparsed_block.flags,
            "Block {block_number} flags mismatch"
        );
        assert!(
            matches!(
                (&block.crc_type, &reparsed_block.crc_type),
                (crc::CrcType::None, crc::CrcType::None)
                    | (crc::CrcType::CRC16_X25, crc::CrcType::CRC16_X25)
                    | (
                        crc::CrcType::CRC32_CASTAGNOLI,
                        crc::CrcType::CRC32_CASTAGNOLI
                    )
            ),
            "Block {block_number} CRC type mismatch"
        );
        assert_eq!(
            block.bib, reparsed_block.bib,
            "Block {block_number} BIB coverage mismatch"
        );
        assert_eq!(
            block.bcb, reparsed_block.bcb,
            "Block {block_number} BCB mismatch"
        );
        assert_eq!(
            block.extent, reparsed_block.extent,
            "Block {block_number} extent mismatch"
        );
        assert_eq!(
            block.data, reparsed_block.data,
            "Block {block_number} data range mismatch"
        );
        assert!(
            block.extent.end <= data.len() as u64,
            "Block {block_number} extent exceeds data length"
        );
        assert!(
            block.data.end <= data.len() as u64,
            "Block {block_number} data range exceeds data length"
        );
    }
}

#[test]
fn rebuild_bundle_no_op() {
    let (bundle, data) = make_bundle();
    let (new_bundle, new_data) = Editor::new(&bundle, &data)
        .rebuild_bundle()
        .map(|(b, c)| (b, Chunk::flatten(c, &data)))
        .unwrap();
    assert_rebuild_matches_parse(&new_bundle, &new_data);
}

#[test]
fn rebuild_bundle_change_destination() {
    let (bundle, data) = make_bundle();
    let new_dest: eid::Eid = "ipn:99.0".parse().unwrap();
    let (new_bundle, new_data) = ok(Editor::new(&bundle, &data).with_destination(new_dest.clone()))
        .rebuild_bundle()
        .map(|(b, c)| (b, Chunk::flatten(c, &data)))
        .unwrap();
    assert_eq!(new_bundle.primary.destination, new_dest);
    assert_eq!(new_bundle.primary.id.source, bundle.primary.id.source);
    assert_rebuild_matches_parse(&new_bundle, &new_data);
}

#[test]
fn rebuild_bundle_multiple_primary_changes() {
    let (bundle, data) = make_bundle();
    let new_dest: eid::Eid = "ipn:99.0".parse().unwrap();
    let new_lifetime = core::time::Duration::from_secs(600);
    let editor = ok(Editor::new(&bundle, &data).with_destination(new_dest.clone()));
    let (new_bundle, new_data) = ok(editor.with_lifetime(new_lifetime))
        .rebuild_bundle()
        .map(|(b, c)| (b, Chunk::flatten(c, &data)))
        .unwrap();
    assert_eq!(new_bundle.primary.destination, new_dest);
    assert_eq!(new_bundle.primary.lifetime, new_lifetime);
    assert_rebuild_matches_parse(&new_bundle, &new_data);
}

#[test]
fn rebuild_bundle_add_block() {
    let (bundle, data) = make_bundle();
    let (new_bundle, new_data) =
        ok(Editor::new(&bundle, &data).push_block(block::Type::Unrecognised(200)))
            .with_data((&[0xCA, 0xFE][..]).into())
            .rebuild()
            .rebuild_bundle()
            .map(|(b, c)| (b, Chunk::flatten(c, &data)))
            .unwrap();
    assert!(new_bundle.blocks.contains_key(&2));
    assert_rebuild_matches_parse(&new_bundle, &new_data);
}

#[test]
fn rebuild_bundle_remove_block() {
    let (bundle, data) = make_bundle_with_hop_count();
    let hop_block = bundle
        .blocks
        .iter()
        .find(|(_, b)| matches!(b.block_type, block::Type::HopCount))
        .map(|(n, _)| *n)
        .expect("Should have hop count block");

    let (new_bundle, new_data) = ok(Editor::new(&bundle, &data).remove_block(hop_block))
        .rebuild_bundle()
        .map(|(b, c)| (b, Chunk::flatten(c, &data)))
        .unwrap();
    assert!(!new_bundle.blocks.contains_key(&hop_block));
    assert_rebuild_matches_parse(&new_bundle, &new_data);
}

#[test]
fn flatten_inplace_no_op() {
    let (bundle, data) = make_bundle();
    let flattened = Editor::new(&bundle, &data)
        .rebuild()
        .map(|c| Chunk::flatten(c, &data))
        .unwrap();
    let chunks = Editor::new(&bundle, &data).rebuild().unwrap();
    let mut inplace = data.to_vec();
    Chunk::flatten_inplace(chunks, &mut inplace);
    assert_eq!(&*flattened, &*inplace);
}

#[test]
fn flatten_inplace_change_destination() {
    let (bundle, data) = make_bundle();
    let new_dest: eid::Eid = "ipn:99.0".parse().unwrap();

    let flattened = ok(Editor::new(&bundle, &data).with_destination(new_dest.clone()))
        .rebuild()
        .map(|c| Chunk::flatten(c, &data))
        .unwrap();

    let chunks = ok(Editor::new(&bundle, &data).with_destination(new_dest))
        .rebuild()
        .unwrap();
    let mut inplace = data.to_vec();
    Chunk::flatten_inplace(chunks, &mut inplace);
    assert_eq!(&*flattened, &*inplace);
}

#[test]
fn flatten_inplace_add_block() {
    let (bundle, data) = make_bundle();

    let flattened = ok(Editor::new(&bundle, &data).push_block(block::Type::Unrecognised(200)))
        .with_data((&[0xCA, 0xFE][..]).into())
        .rebuild()
        .rebuild()
        .map(|c| Chunk::flatten(c, &data))
        .unwrap();

    let chunks = ok(Editor::new(&bundle, &data).push_block(block::Type::Unrecognised(200)))
        .with_data((&[0xCA, 0xFE][..]).into())
        .rebuild()
        .rebuild()
        .unwrap();
    let mut inplace = data.to_vec();
    Chunk::flatten_inplace(chunks, &mut inplace);
    assert_eq!(&*flattened, &*inplace);
}

#[test]
fn flatten_inplace_remove_block() {
    let (bundle, data) = make_bundle_with_hop_count();
    let hop_block = bundle
        .blocks
        .iter()
        .find(|(_, b)| matches!(b.block_type, block::Type::HopCount))
        .map(|(n, _)| *n)
        .expect("Should have hop count block");

    let flattened = ok(Editor::new(&bundle, &data).remove_block(hop_block))
        .rebuild()
        .map(|c| Chunk::flatten(c, &data))
        .unwrap();

    let chunks = ok(Editor::new(&bundle, &data).remove_block(hop_block))
        .rebuild()
        .unwrap();
    let mut inplace = data.to_vec();
    Chunk::flatten_inplace(chunks, &mut inplace);
    assert_eq!(&*flattened, &*inplace);
}

#[test]
fn flatten_inplace_mixed_shift() {
    let long: eid::Eid = "dtn://a-long-destination-endpoint.example.org/svc"
        .parse()
        .unwrap();
    let (_, data) = builder::Builder::new("ipn:1.0".parse().unwrap(), long)
        .with_hop_count(&hop_info::HopInfo {
            limit: 30,
            count: 1,
        })
        .with_payload("payload-bytes-here".as_bytes().into())
        .build(creation_timestamp::CreationTimestamp::now())
        .unwrap();
    let bundle = reparse(&data);
    let short: eid::Eid = "ipn:2.0".parse().unwrap();

    let flattened = ok(
        ok(Editor::new(&bundle, &data).with_destination(short.clone()))
            .push_block(block::Type::PreviousNode),
    )
    .with_data(vec![0xAA; 64].into())
    .rebuild()
    .rebuild()
    .map(|c| Chunk::flatten(c, &data))
    .unwrap();

    let chunks = ok(ok(Editor::new(&bundle, &data).with_destination(short))
        .push_block(block::Type::PreviousNode))
    .with_data(vec![0xAA; 64].into())
    .rebuild()
    .rebuild()
    .unwrap();
    let mut inplace = data.to_vec();
    Chunk::flatten_inplace(chunks, &mut inplace);
    assert_eq!(&*flattened, &*inplace);
}

// R-3: Editor::remove_block must reject a security block (BIB/BCB). Removing a
// BCB directly would leave its targets holding ciphertext with no covering BCB
// (ciphertext surfaced as plaintext on reparse); removing a BIB would silently
// strip integrity. Security blocks are managed only via Signer/Encryptor
// (remove_integrity/remove_encryption), as push/insert/update_block also enforce.
#[test]
fn remove_block_rejects_security_block() {
    let (_, bundle_bytes) =
        builder::Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_payload(b"remove-bib test".as_slice().into())
            .build(creation_timestamp::CreationTimestamp::now())
            .unwrap();
    let bundle = reparse(&bundle_bytes);

    let kek: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "HS256+A128KW",
        "key_ops": ["sign", "verify", "wrapKey", "unwrapKey"],
        "k": "AAAAAAAAAAAAAAAAAAAAAA"
    }))
    .unwrap();

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

    let signed = reparse(&signed_bytes);
    let bib_num = signed
        .blocks
        .iter()
        .find(|(_, b)| matches!(b.block_type, block::Type::BlockIntegrity))
        .map(|(n, _)| *n)
        .expect("Signed bundle should contain a BIB block");

    let result = Editor::new(&signed, &signed_bytes).remove_block(bib_num);
    assert!(
        matches!(result, Err((_, Error::SecurityBlock))),
        "remove_block must reject a BIB block with Error::SecurityBlock"
    );
}
