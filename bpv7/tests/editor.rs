//! Integration tests for `hardy_bpv7::editor::Editor` and
//! `hardy_bpv7::extension_editor::ExtensionEditor` — building, mutating,
//! and rebuilding bundles through the public API.

use core::{num::NonZeroU8, time::Duration};
use std::collections::{HashMap, HashSet};

use bytes::Bytes;
use hardy_bpv7::{
    Bundle, block,
    bpsec::{edit::BPSecEditor, encryptor, key, rfc9173::ScopeFlags, signer},
    builder, bundle, checks, crc, creation_timestamp,
    editor::{Chunk, Editor, Error},
    eid,
    extension_editor::{self, ExtensionEditor},
    hop_info, parse,
};
// Aliased: the parser's error, beside the editor's `Error` imported above.
use hardy_bpv7::Error as Bpv7Error;
// Aliased: the CBOR codec's error, beside the two above.
use hardy_cbor::{decode::Error as CborError, encode::emit};

mod common;
use self::common::rand_k;

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

// Build a bundle with a hop count block, then re-parse to get a fully-parsed
// Bundle with real wire extents.
fn make_bundle_with_hop_count() -> (Bundle, Box<[u8]>) {
    let (_, data) = builder::Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
        .with_hop_count(&hop_info::HopInfo {
            limit: NonZeroU8::new(30).unwrap(),
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
    parse::parse(Bytes::copy_from_slice(data))
        .unwrap_or_else(|e| panic!("the rebuilt bundle must re-parse: {e:?}"))
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
    let new_lifetime = Duration::from_secs(7200);
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
    let new_lifetime = Duration::from_secs(600);
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
    // Equality compares the encoding, so canonical form is checked apart:
    // the rebuilt view holds what the bytes hold, not an alias of it.
    assert!(
        bundle.primary.flags.is_canonical(),
        "primary flags not canonical"
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
        assert!(
            block.block_type.is_canonical() && block.flags.is_canonical(),
            "Block {block_number} type or flags not canonical"
        );
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

// The owner editor canonicalizes the flags it is handed, so the `Bundle`
// `rebuild_bundle()` returns holds what the bytes hold: the admin-record
// bit carried in `unrecognised` makes an administrative record while a
// genuinely unrecognised bit passes through, and `report_on_failure`'s bit
// makes an inserted block report.
#[test]
fn rebuild_bundle_canonicalizes_flag_aliases() {
    let (bundle, data) = make_bundle();
    let aliased = bundle::Flags {
        unrecognised: (1 << 1) | (1 << 24),
        ..Default::default()
    };
    let (new_bundle, new_data) = ok(Editor::new(&bundle, &data).with_bundle_flags(aliased))
        .rebuild_bundle()
        .map(|(b, c)| (b, Chunk::flatten(c, &data)))
        .unwrap();
    assert!(new_bundle.primary.flags.is_admin_record);
    assert_eq!(new_bundle.primary.flags.unrecognised, 1 << 24);
    assert_rebuild_matches_parse(&new_bundle, &new_data);

    let editor = ok(Editor::new(&bundle, &data).insert_block(block::Type::Unrecognised(200)));
    let inserted = editor.block_number();
    let (new_bundle, new_data) = editor
        .with_flags(block::Flags {
            unrecognised: 1 << 1,
            ..Default::default()
        })
        .with_data((&[0x01, 0x02][..]).into())
        .rebuild()
        .rebuild_bundle()
        .map(|(b, c)| (b, Chunk::flatten(c, &data)))
        .unwrap();
    assert!(new_bundle.blocks[&inserted].flags.report_on_failure);
    assert!(new_bundle.blocks[&inserted].flags.is_canonical());
    assert_eq!(
        new_bundle.blocks[&inserted].flags,
        reparse(&new_data).blocks[&inserted].flags
    );
}

#[test]
fn rebuild_bundle_multiple_primary_changes() {
    let (bundle, data) = make_bundle();
    let new_dest: eid::Eid = "ipn:99.0".parse().unwrap();
    let new_lifetime = Duration::from_secs(600);
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
            limit: NonZeroU8::new(30).unwrap(),
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
        "k": rand_k(16)
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

// A primary edit is refused while a BIB covers the primary block, including
// after another target was stripped from that BIB: the strip rewrites the
// BIB, but the primary stays under it, and the edit would break the result.
#[test]
fn primary_edit_refused_while_a_bib_covers_it() {
    let (bundle, data, hop_count, _, _) = make_signed_hop_count(true);
    let new_dest: eid::Eid = "ipn:9.0".parse().unwrap();

    assert!(matches!(
        Editor::new(&bundle, &data).with_destination(new_dest.clone()),
        Err((_, Error::PrimaryBlockHasBib))
    ));

    let stripped = ok(Editor::new(&bundle, &data).update_block(hop_count))
        .with_data(
            emit(&hop_info::HopInfo {
                limit: NonZeroU8::new(30).unwrap(),
                count: 1,
            })
            .0
            .into(),
        )
        .rebuild();
    assert!(matches!(
        stripped.with_destination(new_dest),
        Err((_, Error::PrimaryBlockHasBib))
    ));
}

// `remove_integrity(0)` releases the primary block for editing. The BIB
// keeps the Hop Count's operation, whose scope leaves the primary block
// out, so it still verifies after the edit.
#[test]
fn remove_integrity_releases_the_primary() {
    let (bundle, data, hop_count, bib, key) = make_signed_hop_count(true);
    let new_dest: eid::Eid = "ipn:9.0".parse().unwrap();

    let edited = ok(Editor::new(&bundle, &data).remove_integrity(0));
    let edited = ok(edited.with_destination(new_dest.clone()))
        .rebuild()
        .map(|c| Chunk::flatten(c, &data))
        .expect("rebuild the edited bundle");
    let parse::Parsed {
        data: edited,
        bundle: edited_bundle,
        bibs,
        ..
    } = parse::parse(Bytes::copy_from_slice(&edited)).expect("parse the edited bundle");
    assert_eq!(edited_bundle.primary.destination, new_dest);
    assert_eq!(edited_bundle.blocks[&0].bib, block::BibCoverage::None);
    assert_eq!(
        edited_bundle.blocks[&hop_count].bib,
        block::BibCoverage::Some(bib)
    );
    let deferred = checks::verify_all_bibs(
        &edited,
        &key::KeySet::new(vec![key]),
        &edited_bundle.blocks,
        &bibs,
        &HashMap::new(),
        &HashMap::new(),
    )
    .expect("the Hop Count's operation verifies");
    assert!(deferred.is_empty(), "every target is resident");
}

// Removing the BIB through `BPSecEditor::remove_blocks` releases the
// primary block too, with no key: the BIB is not encrypted.
#[test]
fn removing_the_bib_releases_the_primary() {
    let (bundle, data, _, bib, _) = make_signed_hop_count(true);
    let new_dest: eid::Eid = "ipn:9.0".parse().unwrap();

    let (edited, removed) = Editor::new(&bundle, &data)
        .remove_blocks(HashSet::from([bib]), &key::KeySet::new(Vec::new()))
        .map_err(|(_, e)| e)
        .expect("remove the BIB");
    assert_eq!(removed, HashSet::from([bib]));
    let edited = ok(edited.with_destination(new_dest.clone()))
        .rebuild()
        .map(|c| Chunk::flatten(c, &data))
        .expect("rebuild the edited bundle");
    assert_eq!(reparse(&edited).primary.destination, new_dest);
}

// RFC 9171 §4.2.3-4/-5 against the final primary: the owner editor refuses
// a forbidden `report_on_failure` when it rebuilds, whichever order the
// flag and the forbidding primary were set in, and whether an edit set the
// flag or a kept block carried it under the primary the edits replaced.
#[test]
fn owner_editor_refuses_the_forbidden_flag_at_rebuild() {
    fn with_reporting_block(editor: Editor<'_>) -> Editor<'_> {
        ok(editor.push_block(block::Type::Unrecognised(200)))
            .with_flags(block::Flags {
                report_on_failure: true,
                ..Default::default()
            })
            .with_data(b"ext-data".as_slice().into())
            .rebuild()
    }
    fn refused(result: Result<(Bundle, Vec<Chunk>), Error>, block_number: u64) {
        assert!(
            matches!(result, Err(Error::ReportOnFailureForbidden(n)) if n == block_number),
            "refused, naming block {block_number}"
        );
    }
    // The pushed block takes the first free number: 2 on a bundle of a
    // primary and a payload.
    let pushed = 2;
    let admin = bundle::Flags {
        is_admin_record: true,
        ..Default::default()
    };

    // The flag set on a bundle that already forbids it.
    let (bundle, data) = make_bundle_from(
        "dtn:none",
        bundle::Flags {
            do_not_fragment: true,
            ..Default::default()
        },
    );
    refused(
        with_reporting_block(Editor::new(&bundle, &data)).rebuild_bundle(),
        pushed,
    );

    // The flag set first, then the primary made to forbid it.
    let (bundle, data) = make_bundle();
    let editor = with_reporting_block(Editor::new(&bundle, &data));
    refused(
        ok(editor.with_bundle_flags(admin.clone())).rebuild_bundle(),
        pushed,
    );
    let editor = with_reporting_block(Editor::new(&bundle, &data));
    refused(
        ok(editor.with_source(eid::Eid::Null)).rebuild_bundle(),
        pushed,
    );

    // A kept block: the Hop Count reports on an ordinary bundle, then the
    // bundle becomes an administrative record. Both rebuild paths refuse.
    let (bundle, data) = make_bundle_with_hop_count();
    let hop_count = *bundle
        .blocks
        .iter()
        .find(|(_, b)| b.block_type == block::Type::HopCount)
        .expect("the bundle carries a Hop Count block")
        .0;
    refused(
        ok(Editor::new(&bundle, &data).with_bundle_flags(admin.clone())).rebuild_bundle(),
        hop_count,
    );
    assert!(matches!(
        ok(Editor::new(&bundle, &data).with_bundle_flags(admin.clone())).rebuild(),
        Err(Error::ReportOnFailureForbidden(n)) if n == hop_count
    ));

    // Three flagged blocks, the kept Hop Count and two pushed above it: the
    // refusal names the lowest, whatever the iteration order. Each fresh
    // editor iterates its blocks in its own hash order, so the loop covers
    // many orders on both rebuild paths.
    assert_eq!(hop_count, 2, "precondition: the Hop Count holds block 2");
    for _ in 0..16 {
        let editor = with_reporting_block(with_reporting_block(Editor::new(&bundle, &data)));
        refused(
            ok(editor.with_bundle_flags(admin.clone())).rebuild_bundle(),
            hop_count,
        );
        let editor = with_reporting_block(with_reporting_block(Editor::new(&bundle, &data)));
        assert!(matches!(
            ok(editor.with_bundle_flags(admin.clone())).rebuild(),
            Err(Error::ReportOnFailureForbidden(n)) if n == hop_count
        ));
    }

    // Control: an ordinary bundle keeps the flag.
    let (bundle, data) = make_bundle();
    let (rebuilt, chunks) = with_reporting_block(Editor::new(&bundle, &data))
        .rebuild_bundle()
        .expect("an ordinary bundle permits the flag");
    assert_rebuild_matches_parse(&rebuilt, &Chunk::flatten(chunks, &data));
}

// `Type::canonicalize` folds an `Unrecognised` alias of a known code back
// to the named variant it encodes as, and leaves everything else alone.
// Matched structurally: equality already counts an alias as its variant.
#[test]
fn canonicalize_folds_reserved_aliases() {
    assert!(matches!(
        block::Type::Unrecognised(0).canonicalize(),
        block::Type::Primary
    ));
    assert!(matches!(
        block::Type::Unrecognised(11).canonicalize(),
        block::Type::BlockIntegrity
    ));
    assert!(matches!(
        block::Type::Unrecognised(12).canonicalize(),
        block::Type::BlockSecurity
    ));
    assert!(matches!(
        block::Type::Unrecognised(192).canonicalize(),
        block::Type::Unrecognised(192)
    ));
    assert!(matches!(
        block::Type::Payload.canonicalize(),
        block::Type::Payload
    ));
    assert!(!block::Type::Unrecognised(11).is_canonical());
    assert!(block::Type::Unrecognised(192).is_canonical());
    assert!(block::Type::BlockIntegrity.is_canonical());
}

// Reserved wire codes must be refused whatever `Type` variant carries them:
// `Unrecognised(v)` encodes as the raw code `v`, so a hand-built
// `Unrecognised(11)` would otherwise emit a block the next node parses as a
// real BIB.
#[test]
fn push_block_rejects_reserved_wire_codes() {
    let (bundle, data) = make_bundle();

    let result = Editor::new(&bundle, &data).push_block(block::Type::Unrecognised(0));
    assert!(matches!(result, Err((_, Error::PrimaryBlock))));

    let result = Editor::new(&bundle, &data).push_block(block::Type::Unrecognised(11));
    assert!(matches!(result, Err((_, Error::SecurityBlock))));

    let result = Editor::new(&bundle, &data).push_block(block::Type::Unrecognised(12));
    assert!(matches!(result, Err((_, Error::SecurityBlock))));
}

// The singleton rules see the canonicalized type too: `Unrecognised(1)` is
// a second payload block, whatever the variant says (and the refusal names
// the canonical type).
#[test]
fn push_block_rejects_singleton_duplicates_by_wire_code() {
    let (bundle, data) = make_bundle();
    let result = Editor::new(&bundle, &data).push_block(block::Type::Unrecognised(1));
    assert!(matches!(
        result,
        Err((_, Error::IllegalDuplicate(block::Type::Payload)))
    ));

    let (bundle, data) = make_bundle_with_hop_count();
    let result = Editor::new(&bundle, &data).push_block(block::Type::Unrecognised(10));
    assert!(matches!(
        result,
        Err((_, Error::IllegalDuplicate(block::Type::HopCount)))
    ));
}

// `insert_block` is the third caller-supplied-type door: reserved aliases
// must be refused there too, and the replace-by-type match must compare
// canonical types (an alias must replace the block it aliases, never
// allocate a duplicate the receiving parser rejects).
#[test]
fn insert_block_rejects_reserved_wire_codes() {
    let (bundle, data) = make_bundle();

    let result = Editor::new(&bundle, &data).insert_block(block::Type::Unrecognised(0));
    assert!(matches!(result, Err((_, Error::PrimaryBlock))));

    let result = Editor::new(&bundle, &data).insert_block(block::Type::Unrecognised(11));
    assert!(matches!(result, Err((_, Error::SecurityBlock))));

    let result = Editor::new(&bundle, &data).insert_block(block::Type::Unrecognised(12));
    assert!(matches!(result, Err((_, Error::SecurityBlock))));
}

#[test]
fn insert_block_replaces_via_alias_not_duplicates() {
    let (bundle, data) = make_bundle_with_hop_count();

    let editor = Editor::new(&bundle, &data)
        .insert_block(block::Type::Unrecognised(10))
        .map_err(|(_, e)| e)
        .unwrap()
        .with_data(b"\x82\x18\x1e\x01".as_slice().into())
        .rebuild();
    let new_data = editor.rebuild().map(|c| Chunk::flatten(c, &data)).unwrap();

    let reparsed = reparse(&new_data);
    let hop_blocks = reparsed
        .blocks
        .values()
        .filter(|b| matches!(b.block_type, block::Type::HopCount))
        .count();
    assert_eq!(
        hop_blocks, 1,
        "an alias insert must replace the aliased singleton, not duplicate it"
    );
}

// The primary-block refusal at the Builder door compares canonically too:
// `Unrecognised(0)` encodes as type code 0 and must not bypass the variant
// it aliases. (The builder deliberately permits security blocks and
// duplicates — it is the test-crafting door — so only code 0 is refused.)
#[test]
fn add_extension_block_rejects_wire_code_zero() {
    let r = builder::Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
        .add_extension_block(block::Type::Unrecognised(0));
    assert!(matches!(r, Err(builder::Error::PrimaryBlock)));
}

// === ExtensionEditor: the scoped extension-block handle =================

// A bundle with one Unrecognised(200) extension block, re-parsed to wire
// extents: (bundle, data, extension block number).
fn make_bundle_with_extension() -> (Bundle, Box<[u8]>, u64) {
    let (bundle, data) = make_bundle();
    let new_data = ok(Editor::new(&bundle, &data).push_block(block::Type::Unrecognised(200)))
        .with_data(b"ext-data".as_slice().into())
        .rebuild()
        .rebuild()
        .map(|c| Chunk::flatten(c, &data))
        .unwrap();
    let bundle = reparse(&new_data);
    let ext = bundle
        .blocks
        .iter()
        .find(|(_, b)| matches!(b.block_type, block::Type::Unrecognised(200)))
        .map(|(n, _)| *n)
        .expect("the extension block is present");
    (bundle, new_data, ext)
}

// The extension block signed with a BIB: (bundle, data, extension block
// number, BIB block number, the signing key).
fn make_signed_extension() -> (Bundle, Box<[u8]>, u64, u64, key::Key) {
    let (bundle, data, ext) = make_bundle_with_extension();
    let kek: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "HS256+A128KW",
        "key_ops": ["sign", "verify", "wrapKey", "unwrapKey"],
        "k": rand_k(16)
    }))
    .unwrap();
    let signed_bytes = signer::Signer::new(&bundle, &data)
        .sign_block(
            ext,
            signer::Context::HMAC_SHA2(ScopeFlags::default()),
            "ipn:2.1".parse().unwrap(),
            &kek,
        )
        .map_err(|(_, e)| e)
        .expect("sign the extension block")
        .rebuild()
        .expect("rebuild signed");
    let signed = reparse(&signed_bytes);
    let bib = signed
        .blocks
        .iter()
        .find(|(_, b)| matches!(b.block_type, block::Type::BlockIntegrity))
        .map(|(n, _)| *n)
        .expect("the BIB is present");
    (signed, signed_bytes, ext, bib, kek)
}

#[test]
fn extension_editor_refuses_reserved_insert_types() {
    let (bundle, data) = make_bundle();
    let mut editor = ExtensionEditor::new(&bundle, &data);
    // Each reserved type, named and as the `Unrecognised` alias of its wire
    // code: the alias is refused as the reserved type it encodes.
    for (requested, reserved) in [
        (block::Type::Primary, block::Type::Primary),
        (block::Type::Payload, block::Type::Payload),
        (block::Type::BlockIntegrity, block::Type::BlockIntegrity),
        (block::Type::BlockSecurity, block::Type::BlockSecurity),
        (block::Type::Unrecognised(0), block::Type::Primary),
        (block::Type::Unrecognised(1), block::Type::Payload),
        (block::Type::Unrecognised(11), block::Type::BlockIntegrity),
        (block::Type::Unrecognised(12), block::Type::BlockSecurity),
    ] {
        assert!(
            matches!(
                editor.insert(
                    requested,
                    block::Flags::default(),
                    crc::CrcType::None,
                    b"x".as_slice().into(),
                ),
                Err(extension_editor::Error::ReservedType(t)) if t == reserved
            ),
            "{requested:?} must be refused as ReservedType({reserved:?})"
        );
    }
    assert!(!editor.is_modified(), "refusals must not count as edits");
}

#[test]
fn extension_editor_maps_singleton_duplicates_through() {
    let (bundle, data) = make_bundle_with_hop_count();
    let mut editor = ExtensionEditor::new(&bundle, &data);
    assert!(matches!(
        editor.insert(
            block::Type::HopCount,
            block::Flags::default(),
            crc::CrcType::None,
            hop_count_body(),
        ),
        Err(extension_editor::Error::Editor(Error::IllegalDuplicate(
            block::Type::HopCount
        )))
    ));
    // A refusal from the inner editor is not an edit either.
    assert!(!editor.is_modified());
    assert!(
        editor
            .finish()
            .expect("an untouched editor is not an error")
            .is_none(),
        "a refused insert materialises nothing"
    );
}

#[test]
fn extension_editor_reserves_primary_and_payload_targets() {
    let (bundle, data) = make_bundle();
    let mut editor = ExtensionEditor::new(&bundle, &data);
    for reserved in [0, 1] {
        assert!(matches!(
            editor.replace(reserved, b"x".as_slice().into()),
            Err(extension_editor::Error::ReservedBlock(n)) if n == reserved
        ));
        assert!(matches!(
            editor.remove(reserved),
            Err(extension_editor::Error::ReservedBlock(n)) if n == reserved
        ));
    }

    // The type refuses, not the mechanism: the full Editor replaces the
    // payload under its owner semantics.
    ok(Editor::new(&bundle, &data).update_block(1));
}

#[test]
fn extension_editor_reports_missing_targets() {
    let (bundle, data) = make_bundle();
    let mut editor = ExtensionEditor::new(&bundle, &data);
    assert!(matches!(
        editor.replace(99, b"x".as_slice().into()),
        Err(extension_editor::Error::NoSuchBlock(99))
    ));
}

#[test]
fn extension_editor_refuses_security_blocks_and_covered_targets() {
    let (signed, signed_bytes, ext, bib, _) = make_signed_extension();
    let mut editor = ExtensionEditor::new(&signed, &signed_bytes);

    // The BIB itself is out of scope...
    assert!(matches!(
        editor.replace(bib, b"x".as_slice().into()),
        Err(extension_editor::Error::ReservedType(
            block::Type::BlockIntegrity
        ))
    ));
    // ...and so is its covered target, in both directions.
    assert!(matches!(
        editor.replace(ext, b"x".as_slice().into()),
        Err(extension_editor::Error::Covered(n)) if n == ext
    ));
    assert!(matches!(
        editor.remove(ext),
        Err(extension_editor::Error::Covered(n)) if n == ext
    ));

    // The type refuses, not the mechanism: the full Editor edits the
    // covered target by stripping it from the BIB — an owner decision.
    ok(Editor::new(&signed, &signed_bytes).update_block(ext));
}

#[test]
fn extension_editor_refuses_bcb_covered_targets() {
    // BCB coverage alone (no BIB anywhere) also refuses.
    let (bundle, data, ext) = make_bundle_with_extension();
    let enc_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "A128KW",
        "enc": "A128GCM",
        "key_ops": ["encrypt", "decrypt", "wrapKey", "unwrapKey"],
        "k": rand_k(16)
    }))
    .unwrap();
    let flags = ScopeFlags {
        include_security_header: false,
        ..ScopeFlags::default()
    };
    let encrypted_bytes = encryptor::Encryptor::new(&bundle, &data)
        .encrypt_block(
            ext,
            encryptor::Context::AES_GCM(flags),
            "ipn:2.1".parse().unwrap(),
            &enc_key,
        )
        .map_err(|(_, e)| e)
        .expect("encrypt the extension block")
        .rebuild()
        .expect("rebuild encrypted");
    let encrypted = reparse(&encrypted_bytes);
    let ext_block = encrypted.blocks.get(&ext).unwrap();
    assert!(
        matches!(ext_block.bib, block::BibCoverage::None) && ext_block.bcb.is_some(),
        "BCB-covered with no BIB in sight"
    );

    let mut editor = ExtensionEditor::new(&encrypted, &encrypted_bytes);
    assert!(matches!(
        editor.replace(ext, b"x".as_slice().into()),
        Err(extension_editor::Error::Covered(n)) if n == ext
    ));
    assert!(matches!(
        editor.remove(ext),
        Err(extension_editor::Error::Covered(n)) if n == ext
    ));
}

#[test]
fn extension_editor_refuses_hidden_targets_not_bystanders() {
    // Encrypting a signed block also encrypts its covering BIB (and, per
    // the RFC 9172 cascade, the BIB's other targets). The parser then
    // cannot read the encrypted BIB's target list, so it sweeps the
    // BCB-covered blocks — the only ones a conformant encrypted BIB can
    // target — to BibCoverage::Maybe, and those refuse. A bystander no BCB
    // covers cannot be a hidden target: it stays uncovered and editable.
    let (bundle, data, ext) = make_bundle_with_extension();

    // The bystander: a second extension block nobody signs or encrypts.
    let bystander_data = ok(Editor::new(&bundle, &data).push_block(block::Type::Unrecognised(201)))
        .with_data(b"bystander".as_slice().into())
        .rebuild()
        .rebuild()
        .map(|c| Chunk::flatten(c, &data))
        .unwrap();
    let bundle = reparse(&bystander_data);
    let bystander = bundle
        .blocks
        .iter()
        .find(|(_, b)| matches!(b.block_type, block::Type::Unrecognised(201)))
        .map(|(n, _)| *n)
        .expect("the bystander block is present");

    let kek: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "HS256+A128KW",
        "key_ops": ["sign", "verify", "wrapKey", "unwrapKey"],
        "k": rand_k(16)
    }))
    .unwrap();
    let signed_bytes = signer::Signer::new(&bundle, &bystander_data)
        .sign_block(
            ext,
            signer::Context::HMAC_SHA2(ScopeFlags::default()),
            "ipn:2.1".parse().unwrap(),
            &kek,
        )
        .map_err(|(_, e)| e)
        .expect("sign the extension block")
        .rebuild()
        .expect("rebuild signed");
    let signed = reparse(&signed_bytes);

    let enc_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "A128KW",
        "enc": "A128GCM",
        "key_ops": ["encrypt", "decrypt", "wrapKey", "unwrapKey"],
        "k": rand_k(16)
    }))
    .unwrap();
    let flags = ScopeFlags {
        include_security_header: false,
        ..ScopeFlags::default()
    };
    let encrypted_bytes = encryptor::Encryptor::new(&signed, &signed_bytes)
        .encrypt_block(
            ext,
            encryptor::Context::AES_GCM(flags),
            "ipn:2.1".parse().unwrap(),
            &enc_key,
        )
        .map_err(|(_, e)| e)
        .expect("encrypt the signed extension block (and so its covering BIB)")
        .rebuild()
        .expect("rebuild encrypted");
    let encrypted = reparse(&encrypted_bytes);

    // The actual hidden target: Maybe, with the cascade's BCB.
    let ext_block = encrypted.blocks.get(&ext).unwrap();
    assert!(
        matches!(ext_block.bib, block::BibCoverage::Maybe) && ext_block.bcb.is_some(),
        "the encrypted BIB's real target is swept to Maybe and BCB-covered"
    );
    // The bystander: no BCB, so no conformant encrypted BIB can target it.
    let bystander_block = encrypted.blocks.get(&bystander).unwrap();
    assert!(
        matches!(bystander_block.bib, block::BibCoverage::None) && bystander_block.bcb.is_none(),
        "the sweep must leave a block no BCB covers uncovered"
    );

    let mut editor = ExtensionEditor::new(&encrypted, &encrypted_bytes);
    assert!(matches!(
        editor.replace(ext, b"x".as_slice().into()),
        Err(extension_editor::Error::Covered(n)) if n == ext
    ));
    editor
        .replace(bystander, b"x".as_slice().into())
        .expect("a bystander is editable");
    let (_, chunks) = editor.finish().expect("materialise").expect("edited");
    let new_data = Chunk::flatten(chunks, &encrypted_bytes);
    let reparsed = reparse(&new_data);
    assert_eq!(
        reparsed
            .blocks
            .get(&bystander)
            .unwrap()
            .payload(&new_data)
            .expect("resident"),
        b"x"
    );
    // The hidden target keeps its protection through the bystander edit.
    let ext_after = reparsed.blocks.get(&ext).unwrap();
    assert!(
        matches!(ext_after.bib, block::BibCoverage::Maybe) && ext_after.bcb.is_some(),
        "the encrypted BIB's real target is still BCB-covered and Maybe"
    );
}

#[test]
fn extension_editor_targets_its_own_inserts() {
    let (bundle, data) = make_bundle();
    let mut editor = ExtensionEditor::new(&bundle, &data);

    let n = editor
        .insert(
            block::Type::Unrecognised(201),
            block::Flags::default(),
            crc::CrcType::None,
            b"first".as_slice().into(),
        )
        .expect("insert an extension block");
    editor
        .replace(n, b"second".as_slice().into())
        .expect("a fresh insert is a valid replace target");
    editor
        .remove(n)
        .expect("a fresh insert is a valid remove target");

    // Everything cancelled out, but edits were applied: the editor
    // materialises, and the output holds no trace of the block.
    assert!(editor.is_modified());
    let (_, chunks) = editor
        .finish()
        .expect("materialise")
        .expect("edits were applied");
    let reparsed = reparse(&Chunk::flatten(chunks, &data));
    assert!(
        !reparsed
            .blocks
            .values()
            .any(|b| matches!(b.block_type, block::Type::Unrecognised(201)))
    );
}

#[test]
fn extension_editor_materialises_inserts_and_skips_untouched() {
    let (bundle, data) = make_bundle();

    let untouched = ExtensionEditor::new(&bundle, &data);
    assert!(!untouched.is_modified());
    assert!(
        untouched
            .finish()
            .expect("no edits is not an error")
            .is_none(),
        "an untouched editor materialises nothing"
    );

    let mut editor = ExtensionEditor::new(&bundle, &data);
    let n = editor
        .insert(
            block::Type::Unrecognised(202),
            block::Flags::default(),
            crc::CrcType::None,
            b"materialised".as_slice().into(),
        )
        .expect("insert");
    let (new_bundle, chunks) = editor.finish().expect("materialise").expect("edited");
    let new_data = Chunk::flatten(chunks, &data);
    let reparsed = reparse(&new_data);
    let block = reparsed.blocks.get(&n).expect("the insert is on the wire");
    assert!(matches!(block.block_type, block::Type::Unrecognised(202)));
    assert_eq!(block.payload(&new_data).expect("resident"), b"materialised");
    assert_eq!(new_bundle.blocks.len(), reparsed.blocks.len());
}

// === ExtensionEditor: the parser's accept-set at call time =============

// A well-formed Hop Count body.
fn hop_count_body() -> Box<[u8]> {
    emit(&hop_info::HopInfo {
        limit: NonZeroU8::new(30).unwrap(),
        count: 0,
    })
    .0
    .into()
}

// A parsed bundle built with the given source and bundle flags.
fn make_bundle_from(source: &str, flags: bundle::Flags) -> (Bundle, Box<[u8]>) {
    let (_, data) = builder::Builder::new(source.parse().unwrap(), "ipn:2.0".parse().unwrap())
        .with_flags(flags)
        .with_payload(b"Hello".as_slice().into())
        .build(creation_timestamp::CreationTimestamp::now())
        .unwrap();
    let bundle = reparse(&data);
    (bundle, data)
}

// Insert an Unrecognised(200) block with `report_on_failure` set.
fn insert_reporting_block(editor: &mut ExtensionEditor) -> extension_editor::Result<u64> {
    editor.insert(
        block::Type::Unrecognised(200),
        block::Flags {
            report_on_failure: true,
            ..Default::default()
        },
        crc::CrcType::None,
        b"ext-data".as_slice().into(),
    )
}

#[test]
fn extension_editor_refuses_report_on_failure_the_bundle_forbids() {
    // RFC 9171 §4.2.3-4/-5: a null-source bundle (which must also be
    // unfragmentable) and an administrative record both forbid the flag.
    let null_source = make_bundle_from(
        "dtn:none",
        bundle::Flags {
            do_not_fragment: true,
            ..Default::default()
        },
    );
    let admin_record = make_bundle_from(
        "ipn:1.0",
        bundle::Flags {
            is_admin_record: true,
            ..Default::default()
        },
    );
    for (bundle, data) in [&null_source, &admin_record] {
        assert!(
            bundle.primary.forbids_report_on_failure(),
            "precondition: the fixture forbids report_on_failure"
        );
        let mut editor = ExtensionEditor::new(bundle, data);
        assert!(matches!(
            insert_reporting_block(&mut editor),
            Err(extension_editor::Error::Invalid(Bpv7Error::InvalidFlags))
        ));
        assert!(!editor.is_modified(), "a refusal is not an edit");
    }

    // Control: an ordinary bundle takes the same insert, and the rewrite
    // re-parses.
    let (bundle, data) = make_bundle();
    assert!(!bundle.primary.forbids_report_on_failure());
    let mut editor = ExtensionEditor::new(&bundle, &data);
    let inserted = insert_reporting_block(&mut editor).expect("the insert is accepted");
    let (_, chunks) = editor.finish().unwrap().expect("an edit materialises");
    let rewritten = reparse(&Chunk::flatten(chunks, &data));
    assert!(rewritten.blocks[&inserted].flags.report_on_failure);
}

// A named bit carried in `unrecognised` is the flag it encodes: the editor
// refuses `report_on_failure`'s alias on an administrative record as the
// forbidden flag, and on an ordinary bundle the alias reports, with a
// genuinely unrecognised bit passing through.
#[test]
fn extension_editor_canonicalizes_unrecognised_aliases_of_named_flags() {
    fn aliased() -> block::Flags {
        block::Flags {
            unrecognised: (1 << 1) | (1 << 8),
            ..Default::default()
        }
    }

    let (bundle, data) = make_bundle_from(
        "ipn:1.0",
        bundle::Flags {
            is_admin_record: true,
            ..Default::default()
        },
    );
    let mut editor = ExtensionEditor::new(&bundle, &data);
    assert!(matches!(
        editor.insert(
            block::Type::Unrecognised(200),
            aliased(),
            crc::CrcType::None,
            b"ext-data".as_slice().into(),
        ),
        Err(extension_editor::Error::Invalid(Bpv7Error::InvalidFlags))
    ));
    assert!(!editor.is_modified(), "a refusal is not an edit");

    let (bundle, data) = make_bundle_from("ipn:1.0", bundle::Flags::default());
    let mut editor = ExtensionEditor::new(&bundle, &data);
    let inserted = editor
        .insert(
            block::Type::Unrecognised(200),
            aliased(),
            crc::CrcType::None,
            b"ext-data".as_slice().into(),
        )
        .expect("an ordinary bundle permits the flag");
    let (_, chunks) = editor.finish().unwrap().expect("an edit materialises");
    let rewritten = reparse(&Chunk::flatten(chunks, &data));
    let flags = &rewritten.blocks[&inserted].flags;
    assert!(flags.report_on_failure);
    assert_eq!(flags.unrecognised, 1 << 8);
}

#[test]
fn extension_editor_refuses_an_unrecognised_crc_type() {
    let (bundle, data) = make_bundle();
    let mut editor = ExtensionEditor::new(&bundle, &data);
    assert!(matches!(
        editor.insert(
            block::Type::Unrecognised(200),
            block::Flags::default(),
            crc::CrcType::Unrecognised(5),
            b"ext-data".as_slice().into(),
        ),
        Err(extension_editor::Error::Invalid(Bpv7Error::InvalidCrc(
            crc::Error::InvalidType(5)
        )))
    ));
    assert!(!editor.is_modified());

    // Control: each recognised CRC type is accepted and the inserted block
    // carries it — the re-parse checks the CRC value too.
    for crc_type in [crc::CrcType::CRC16_X25, crc::CrcType::CRC32_CASTAGNOLI] {
        let mut editor = ExtensionEditor::new(&bundle, &data);
        let inserted = editor
            .insert(
                block::Type::Unrecognised(200),
                block::Flags::default(),
                crc_type,
                b"ext-data".as_slice().into(),
            )
            .expect("a recognised CRC type is accepted");
        let (_, chunks) = editor.finish().unwrap().expect("an edit materialises");
        let rewritten = reparse(&Chunk::flatten(chunks, &data));
        assert_eq!(rewritten.blocks[&inserted].crc_type, crc_type);
    }
}

// Insert `body` as `block_type` into a fresh bundle, expecting a refusal;
// returns the parser error the refusal carries.
fn refused_insert(block_type: block::Type, body: &[u8]) -> Bpv7Error {
    let (bundle, data) = make_bundle();
    let mut editor = ExtensionEditor::new(&bundle, &data);
    let result = editor.insert(
        block_type,
        block::Flags::default(),
        crc::CrcType::None,
        body.into(),
    );
    assert!(!editor.is_modified(), "a refusal is not an edit");
    match result {
        Err(extension_editor::Error::UndecodableBody {
            block_type: checked,
            source,
        }) => {
            assert_eq!(checked, block_type, "the refusal names the type checked");
            *source
        }
        other => panic!("{block_type:?} must be refused as UndecodableBody, got {other:?}"),
    }
}

#[test]
fn extension_editor_refuses_undecodable_well_known_insert_bodies() {
    // Each type's body decoder rejects a CBOR text string ("bad") with its
    // own error, and the refusal carries exactly that error.
    let bad = b"\x63bad".as_slice();
    assert!(matches!(
        refused_insert(block::Type::PreviousNode, bad),
        Bpv7Error::InvalidEid(eid::Error::InvalidCBOR(CborError::IncorrectType(..)))
    ));
    assert!(matches!(
        refused_insert(block::Type::BundleAge, bad),
        Bpv7Error::InvalidCBOR(CborError::IncorrectType(..))
    ));
    assert!(matches!(
        refused_insert(block::Type::HopCount, bad),
        Bpv7Error::InvalidCBOR(CborError::IncorrectType(..))
    ));
    // The `Unrecognised` alias of Hop Count's code is validated as a Hop
    // Count: a bare integer is not the Hop Count array.
    assert!(matches!(
        refused_insert(block::Type::Unrecognised(10), &[0x00]),
        Bpv7Error::InvalidCBOR(CborError::IncorrectType(..))
    ));

    // Control: a valid body of each type is accepted and re-parses.
    let valid_bodies: [(block::Type, Box<[u8]>); 3] = [
        (
            block::Type::PreviousNode,
            emit(&"ipn:3.0".parse::<eid::Eid>().unwrap()).0.into(),
        ),
        (block::Type::BundleAge, emit(&0u64).0.into()),
        (block::Type::HopCount, hop_count_body()),
    ];
    for (block_type, valid) in valid_bodies {
        let (bundle, data) = make_bundle();
        let mut editor = ExtensionEditor::new(&bundle, &data);
        let inserted = editor
            .insert(
                block_type,
                block::Flags::default(),
                crc::CrcType::None,
                valid.clone(),
            )
            .expect("a valid body is accepted");
        let (_, chunks) = editor.finish().unwrap().expect("an edit materialises");
        let rewritten_data = Chunk::flatten(chunks, &data);
        let block = &reparse(&rewritten_data).blocks[&inserted];
        assert_eq!(block.block_type, block_type);
        assert_eq!(block.payload(&rewritten_data), Some(&*valid));
    }
}

#[test]
fn extension_editor_refuses_an_undecodable_well_known_replacement() {
    let (bundle, data) = make_bundle_with_hop_count();
    let hop = bundle
        .blocks
        .iter()
        .find(|(_, b)| matches!(b.block_type, block::Type::HopCount))
        .map(|(n, _)| *n)
        .expect("the hop count is present");
    let mut editor = ExtensionEditor::new(&bundle, &data);
    // Well-formed CBOR, but a hop limit of 0 is outside RFC 9171 §4.4.3's
    // 1..=255: the semantic check refuses it, not just the shape check.
    let Err(extension_editor::Error::UndecodableBody {
        block_type: block::Type::HopCount,
        source,
    }) = editor.replace(hop, [0x82, 0x00, 0x00].as_slice().into())
    else {
        panic!("an out-of-range hop limit must be refused as UndecodableBody");
    };
    assert!(matches!(*source, Bpv7Error::InvalidHopLimit(0)));
    assert!(!editor.is_modified());

    // Control: a valid body is accepted, and the replace changes the data
    // alone — the block keeps its flags and CRC type.
    let replacement = hop_info::HopInfo {
        limit: NonZeroU8::new(30).unwrap(),
        count: 1,
    };
    editor
        .replace(hop, emit(&replacement).0.into())
        .expect("a valid body is accepted");
    let (_, chunks) = editor.finish().unwrap().expect("an edit materialises");
    let rewritten_data = Chunk::flatten(chunks, &data);
    let rewritten = reparse(&rewritten_data);
    let original = &bundle.blocks[&hop];
    let replaced = &rewritten.blocks[&hop];
    assert_eq!(replaced.flags, original.flags);
    assert_eq!(replaced.crc_type, original.crc_type);
    assert_eq!(
        replaced
            .extract::<hop_info::HopInfo>(&rewritten_data)
            .unwrap(),
        Some(replacement)
    );
}

// === insert_block replace-by-type: BIB/BCB coverage parity =============

// A bundle whose Hop Count block is BIB-signed: (bundle, data, Hop Count
// block number, BIB block number, signing key). With `sign_primary` the same
// BIB also signs the primary block, under a scope without the primary block
// flag: RFC 9173's default scope would put the primary's bytes in the Hop
// Count's operation too, and any edit that releases the primary would break
// it.
fn make_signed_hop_count(sign_primary: bool) -> (Bundle, Box<[u8]>, u64, u64, key::Key) {
    let (bundle, data) = make_bundle_with_hop_count();
    let hop = bundle
        .blocks
        .iter()
        .find(|(_, b)| matches!(b.block_type, block::Type::HopCount))
        .map(|(n, _)| *n)
        .expect("the hop count block is present");
    // A two-target BIB signs with a direct key: under key wrap only one
    // target's wrapped key reaches the wire (the bpv7 TODO's multi-target
    // BIB entry).
    let kek: key::Key = if sign_primary {
        serde_json::from_value(serde_json::json!({
            "kty": "oct",
            "alg": "HS256",
            "key_ops": ["sign", "verify"],
            "k": rand_k(32)
        }))
    } else {
        serde_json::from_value(serde_json::json!({
            "kid": "ipn:2.1",
            "kty": "oct",
            "alg": "HS256+A128KW",
            "key_ops": ["sign", "verify", "wrapKey", "unwrapKey"],
            "k": rand_k(16)
        }))
    }
    .unwrap();
    let scope = if sign_primary {
        ScopeFlags {
            include_primary_block: false,
            ..ScopeFlags::default()
        }
    } else {
        ScopeFlags::default()
    };
    let mut signing = signer::Signer::new(&bundle, &data);
    if sign_primary {
        signing = signing
            .sign_block(
                0,
                signer::Context::HMAC_SHA2(scope.clone()),
                "ipn:2.1".parse().unwrap(),
                &kek,
            )
            .map_err(|(_, e)| e)
            .expect("sign the primary block");
    }
    let signed_bytes = signing
        .sign_block(
            hop,
            signer::Context::HMAC_SHA2(scope),
            "ipn:2.1".parse().unwrap(),
            &kek,
        )
        .map_err(|(_, e)| e)
        .expect("sign the hop count block")
        .rebuild()
        .expect("rebuild signed");
    let signed = reparse(&signed_bytes);
    let bib = signed
        .blocks
        .iter()
        .find(|(_, b)| matches!(b.block_type, block::Type::BlockIntegrity))
        .map(|(n, _)| *n)
        .expect("the BIB is present");
    if sign_primary {
        assert_eq!(
            signed.blocks[&0].bib,
            block::BibCoverage::Some(bib),
            "one BIB covers the primary and the Hop Count"
        );
    }
    (signed, signed_bytes, hop, bib, kek)
}

#[test]
fn insert_block_replace_strips_bib_coverage_like_update_block() {
    let (signed, signed_bytes, hop, _, _) = make_signed_hop_count(false);

    // Replace the signed hop count via the replace-by-type door.
    let (rebuilt, chunks) =
        ok(Editor::new(&signed, &signed_bytes).insert_block(block::Type::HopCount))
            .with_data(
                emit(&hop_info::HopInfo {
                    limit: NonZeroU8::new(30).unwrap(),
                    count: 1,
                })
                .0
                .into(),
            )
            .rebuild()
            .rebuild_bundle()
            .expect("rebuild the replaced bundle");

    // In-memory and wire agree: no BIB claims the replaced block.
    assert!(
        matches!(
            rebuilt.blocks.get(&hop).unwrap().bib,
            block::BibCoverage::None
        ),
        "the rebuilt Bundle must not report BIB coverage on the replaced block"
    );
    let new_data = Chunk::flatten(chunks, &signed_bytes);
    let parsed =
        parse::parse(Bytes::copy_from_slice(&new_data)).expect("the emitted wire form parses");
    assert!(
        parsed
            .bibs
            .values()
            .all(|ops| !ops.operations().contains_key(&hop)),
        "no BIB on the wire may still target the block whose body was replaced"
    );
    assert!(
        matches!(
            parsed.bundle.blocks.get(&hop).unwrap().bib,
            block::BibCoverage::None
        ),
        "a reparse agrees the replaced block is uncovered"
    );
}

#[test]
fn insert_block_replace_refuses_an_encrypted_bib() {
    let (signed, signed_bytes, hop, _, _) = make_signed_hop_count(false);

    // Encrypt the signed hop count: the cascade also encrypts its BIB.
    let enc_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "A128KW",
        "enc": "A128GCM",
        "key_ops": ["encrypt", "decrypt", "wrapKey", "unwrapKey"],
        "k": rand_k(16)
    }))
    .unwrap();
    let flags = ScopeFlags {
        include_security_header: false,
        ..ScopeFlags::default()
    };
    let encrypted_bytes = encryptor::Encryptor::new(&signed, &signed_bytes)
        .encrypt_block(
            hop,
            encryptor::Context::AES_GCM(flags),
            "ipn:2.1".parse().unwrap(),
            &enc_key,
        )
        .map_err(|(_, e)| e)
        .expect("encrypt the signed hop count")
        .rebuild()
        .expect("rebuild encrypted");
    let encrypted = reparse(&encrypted_bytes);

    // A structural (keyless) parse cannot prove what the encrypted BIB
    // covers, and the hop count is BCB-covered, so it may be a hidden
    // target: it reads as Maybe, and the replace refuses on unprovable
    // coverage, exactly as update_block does. (BibIsEncrypted is the
    // verify-stamped shape, where coverage is known to be Some.)
    let result = Editor::new(&encrypted, &encrypted_bytes).insert_block(block::Type::HopCount);
    assert!(
        matches!(
            result,
            Err((
                _,
                Error::Builder(builder::Error::InternalError(hardy_bpv7::Error::InvalidBPSec(
                    hardy_bpv7::bpsec::Error::MaybeHasBib(n)
                )))
            )) if n == hop
        ),
        "replacing a block under an encrypted BIB must refuse, as update_block does"
    );
}

#[test]
fn insert_block_replace_proceeds_on_a_bystander() {
    // The forwarder's shape: the payload is signed then encrypted (so its
    // BIB is encrypted too), and the relay must rewrite a hop count no BCB
    // covers. A conformant encrypted BIB cannot target it, so the replace
    // goes ahead and leaves every BPSec block exactly as it found it.
    let (bundle, data) = make_bundle_with_hop_count();
    let hop = bundle
        .blocks
        .iter()
        .find(|(_, b)| matches!(b.block_type, block::Type::HopCount))
        .map(|(n, _)| *n)
        .expect("the hop count block is present");
    let kek: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "HS256+A128KW",
        "key_ops": ["sign", "verify", "wrapKey", "unwrapKey"],
        "k": rand_k(16)
    }))
    .unwrap();
    let signed_bytes = signer::Signer::new(&bundle, &data)
        .sign_block(
            1,
            signer::Context::HMAC_SHA2(ScopeFlags::default()),
            "ipn:2.1".parse().unwrap(),
            &kek,
        )
        .map_err(|(_, e)| e)
        .expect("sign the payload")
        .rebuild()
        .expect("rebuild signed");
    let signed = reparse(&signed_bytes);
    let enc_key: key::Key = serde_json::from_value(serde_json::json!({
        "kid": "ipn:2.1",
        "kty": "oct",
        "alg": "A128KW",
        "enc": "A128GCM",
        "key_ops": ["encrypt", "decrypt", "wrapKey", "unwrapKey"],
        "k": rand_k(16)
    }))
    .unwrap();
    let flags = ScopeFlags {
        include_security_header: false,
        ..ScopeFlags::default()
    };
    let encrypted_bytes = encryptor::Encryptor::new(&signed, &signed_bytes)
        .encrypt_block(
            1,
            encryptor::Context::AES_GCM(flags),
            "ipn:2.1".parse().unwrap(),
            &enc_key,
        )
        .map_err(|(_, e)| e)
        .expect("encrypt the payload (and so its covering BIB)")
        .rebuild()
        .expect("rebuild encrypted");
    let encrypted = reparse(&encrypted_bytes);
    let hop_block = encrypted.blocks.get(&hop).unwrap();
    assert!(
        matches!(hop_block.bib, block::BibCoverage::None) && hop_block.bcb.is_none(),
        "the sweep must leave a hop count no BCB covers uncovered"
    );

    let new_data =
        ok(Editor::new(&encrypted, &encrypted_bytes).insert_block(block::Type::HopCount))
            .with_data(
                emit(&hop_info::HopInfo {
                    limit: NonZeroU8::new(30).unwrap(),
                    count: 1,
                })
                .0
                .into(),
            )
            .rebuild()
            .rebuild()
            .map(|c| Chunk::flatten(c, &encrypted_bytes))
            .expect("the bystander replace rebuilds");
    let replaced = reparse(&new_data);
    let hop_info = replaced
        .blocks
        .get(&hop)
        .unwrap()
        .extract::<hop_info::HopInfo>(&new_data)
        .expect("the hop count decodes")
        .expect("the hop count is resident");
    assert_eq!(hop_info.count, 1);

    // Every block the encrypted BIB or a BCB protects or occupies — the
    // payload, the BIB, the BCBs — is byte-identical after the replace.
    for (n, before) in encrypted.blocks.iter().filter(|(_, b)| {
        b.bcb.is_some()
            || matches!(
                b.block_type,
                block::Type::BlockIntegrity | block::Type::BlockSecurity
            )
    }) {
        let after = replaced.blocks.get(n).expect("the block survives");
        assert_eq!(after.block_type, before.block_type);
        assert_eq!(
            after.payload(&new_data),
            before.payload(&encrypted_bytes),
            "block {n} must be untouched by the bystander replace"
        );
    }
}

// An AES-GCM key for the encryption fixtures below; its value is
// immaterial, so it is generated.
fn aes_key() -> key::Key {
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

// Encrypt block `n` of (bundle, data) with `key`, returning the reparsed
// bundle and its bytes. The security header stays out of the AAD, as in the
// other encryption fixtures here.
fn encrypt(bundle: &Bundle, data: &[u8], n: u64, key: &key::Key) -> (Bundle, Box<[u8]>) {
    let flags = ScopeFlags {
        include_security_header: false,
        ..ScopeFlags::default()
    };
    let bytes = encryptor::Encryptor::new(bundle, data)
        .encrypt_block(
            n,
            encryptor::Context::AES_GCM(flags),
            "ipn:2.1".parse().unwrap(),
            key,
        )
        .map_err(|(_, e)| e)
        .expect("encrypt the block")
        .rebuild()
        .expect("rebuild encrypted");
    (reparse(&bytes), bytes)
}

#[test]
fn insert_block_replace_refuses_a_verified_encrypted_bib() {
    // A keyed node resolves the encrypted BIB's targets: the signed-then-
    // encrypted hop count is stamped as covered by a BIB that is itself
    // BCB-encrypted, and the replace refuses with BibIsEncrypted — the
    // target cannot be stripped from the BIB's ciphertext.
    let (signed, signed_bytes, hop, bib, _) = make_signed_hop_count(false);
    let enc_key = aes_key();
    let (_, encrypted_bytes) = encrypt(&signed, &signed_bytes, hop, &enc_key);

    let parse::Parsed {
        data,
        mut bundle,
        bcbs,
        mut bibs,
    } = parse::parse(Bytes::copy_from_slice(&encrypted_bytes)).expect("parse encrypted");
    // Decrypting the BIB is all it takes to learn its targets; the signing
    // key stays out, since KeySet serves the first key whose operations
    // match and the HS256+A128KW key would answer the BCB's unwrap.
    let keys = key::KeySet::new(vec![enc_key]);
    let failed = checks::decrypt_and_validate_covered_bibs(
        &data,
        &keys,
        &mut bundle.blocks,
        &bcbs,
        &mut bibs,
        &mut HashMap::new(),
        &HashMap::new(),
    )
    .expect("the keyed pass decrypts and structurally checks the BIB");
    assert!(failed.is_empty(), "the BIB must decrypt");
    assert_eq!(
        bundle.blocks.get(&hop).unwrap().bib,
        block::BibCoverage::Some(bib),
        "the keyed pass stamps the hop count's real coverage"
    );
    assert!(
        bundle.blocks.get(&bib).unwrap().bcb.is_some(),
        "the covering BIB is itself encrypted"
    );

    assert!(matches!(
        Editor::new(&bundle, &data).insert_block(block::Type::HopCount),
        Err((_, Error::BibIsEncrypted(n))) if n == hop
    ));
}

#[test]
fn insert_block_replace_of_a_bcb_only_block_needs_fresh_data() {
    // An encrypted hop count no BIB covers: the replace strips it from its
    // BCB and hands back a builder with no data, its body being ciphertext.
    let (bundle, data) = make_bundle_with_hop_count();
    let hop = bundle
        .blocks
        .iter()
        .find(|(_, b)| matches!(b.block_type, block::Type::HopCount))
        .map(|(n, _)| *n)
        .expect("the hop count block is present");
    let (encrypted, encrypted_bytes) = encrypt(&bundle, &data, hop, &aes_key());
    let hop_block = encrypted.blocks.get(&hop).unwrap();
    assert!(
        hop_block.bcb.is_some() && matches!(hop_block.bib, block::BibCoverage::None),
        "the hop count is BCB-covered and nothing else"
    );

    // Flags alone leave the builder data-less, and the rebuild refuses.
    let result = ok(Editor::new(&encrypted, &encrypted_bytes).insert_block(block::Type::HopCount))
        .with_flags(block::Flags::default())
        .rebuild()
        .rebuild();
    assert!(matches!(
        result,
        Err(Error::Builder(builder::Error::NoBlockData))
    ));

    // Fresh data rebuilds, and neither the rebuilt bundle nor the wire
    // keeps the block under its old BCB.
    let new_body = emit(&hop_info::HopInfo {
        limit: NonZeroU8::new(30).unwrap(),
        count: 1,
    })
    .0;
    let (rebuilt, chunks) =
        ok(Editor::new(&encrypted, &encrypted_bytes).insert_block(block::Type::HopCount))
            .with_data(new_body.clone().into())
            .rebuild()
            .rebuild_bundle()
            .expect("rebuild with fresh data");
    assert!(rebuilt.blocks.get(&hop).unwrap().bcb.is_none());
    let new_data = Chunk::flatten(chunks, &encrypted_bytes);
    let parsed =
        parse::parse(Bytes::copy_from_slice(&new_data)).expect("the emitted wire form parses");
    assert!(
        parsed
            .bcbs
            .values()
            .all(|ops| !ops.operations().contains_key(&hop)),
        "no BCB on the wire may still target the replaced block"
    );
    let replaced = parsed.bundle.blocks.get(&hop).unwrap();
    assert!(replaced.bcb.is_none());
    assert_eq!(replaced.payload(&new_data), Some(new_body.as_slice()));
}

#[test]
fn insert_block_replace_keeps_an_uncovered_blocks_flags_and_crc() {
    // The common forwarding case: an uncovered hop count with non-default
    // flags and a CRC. Replacing its data keeps both, and the rebuilt
    // bundle agrees with a reparse.
    let flags = block::Flags {
        must_replicate: true,
        report_on_failure: true,
        delete_block_on_failure: true,
        ..Default::default()
    };
    let (_, data) = builder::Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
        .add_extension_block(block::Type::HopCount)
        .expect("add the hop count")
        .with_flags(flags.clone())
        .with_crc_type(crc::CrcType::CRC32_CASTAGNOLI)
        .build(
            emit(&hop_info::HopInfo {
                limit: NonZeroU8::new(30).unwrap(),
                count: 0,
            })
            .0
            .into(),
        )
        .with_payload("Hello".as_bytes().into())
        .build(creation_timestamp::CreationTimestamp::now())
        .unwrap();
    let bundle = reparse(&data);
    let hop = bundle
        .blocks
        .iter()
        .find(|(_, b)| matches!(b.block_type, block::Type::HopCount))
        .map(|(n, _)| *n)
        .expect("the hop count block is present");

    let (rebuilt, chunks) = ok(Editor::new(&bundle, &data).insert_block(block::Type::HopCount))
        .with_data(
            emit(&hop_info::HopInfo {
                limit: NonZeroU8::new(30).unwrap(),
                count: 1,
            })
            .0
            .into(),
        )
        .rebuild()
        .rebuild_bundle()
        .expect("rebuild the replaced bundle");
    let new_data = Chunk::flatten(chunks, &data);
    assert_rebuild_matches_parse(&rebuilt, &new_data);

    let replaced = reparse(&new_data);
    let hop_block = replaced.blocks.get(&hop).unwrap();
    assert_eq!(hop_block.flags, flags);
    assert_eq!(hop_block.crc_type, crc::CrcType::CRC32_CASTAGNOLI);
    let hop_info = hop_block
        .extract::<hop_info::HopInfo>(&new_data)
        .expect("the hop count decodes")
        .expect("the hop count is resident");
    assert_eq!(hop_info.count, 1);
}
