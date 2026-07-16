//! Integration tests for the structural parser — `hardy_bpv7::parse`'s
//! public-API acceptance and rejection decisions on wire bytes. Keyed
//! BPSec pipeline composition lives in `tests/checks.rs`; the streaming
//! push-parser in `tests/streaming.rs`.

use bytes::Bytes;
use hardy_bpv7::{Error, block, builder, crc, creation_timestamp, hop_info, parse};
use hex_literal::hex;

// Build a minimal valid bundle and return its serialised bytes.
fn build_minimal_bundle() -> Box<[u8]> {
    builder::Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
        .with_payload("Hello".as_bytes().into())
        .build(creation_timestamp::CreationTimestamp::now())
        .unwrap()
        .1
}

// RFC 9171 §4.3.2 is categorical for block-type-specific data: "a single
// definite-length CBOR byte string, i.e., a CBOR byte string that is not of
// indefinite length" — a specific override of §4.1's general
// "indefinite-length items are not prohibited" carve-out. The parser rejects
// an indefinite-length data field by design and deliberately does not
// exercise §4.1's "MAY accept ... and transform it into conformant BP
// structure" robustness clause.
#[test]
fn indefinite_length_block_data_is_rejected() {
    // Primary (ipn EIDs, no CRC) + payload block [1, 1, flags 0, crc 0, data]
    // + outer break.
    let definite = hex!(
        "9f88070000820282010282028202018202820201820018281a000f4240"
        "85010100004548454c4c4f" // data = definite-length bstr "HELLO"
        "ff"
    );
    // Byte-identical except the payload data is an indefinite-length byte
    // string of two chunks, "HEL" + "LO".
    let indefinite = hex!(
        "9f88070000820282010282028202018202820201820018281a000f4240"
        "85010100005f4348454c424c4fff" // data = 0x5f, "HEL", "LO", break
        "ff"
    );

    parse::parse(Bytes::copy_from_slice(&definite)).expect("the definite-length twin must parse");

    let err = match parse::parse(Bytes::copy_from_slice(&indefinite)) {
        Ok(_) => panic!("indefinite-length block data must be rejected"),
        Err(e) => e,
    };
    let Error::InvalidField { source, .. } = &err else {
        panic!("expected InvalidField, got {err:?}");
    };
    assert!(
        matches!(source.downcast_ref::<Error>(), Some(Error::NotCanonical)),
        "expected NotCanonical, got {source:?}"
    );
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
        parse::parse(Bytes::from_static(BUNDLE)),
        Err(Error::InvalidFlags)
    ));
}

// NOTE: LLR 1.1.33 (Bundle Age required when Creation Time is zero) is now enforced
// by the BPA rfc9171-filter, not the parser. Parser accepts such bundles to allow
// compatibility with RFC9173 test vectors.

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

    let parse::Parsed {
        data,
        bundle: raw_bundle,
        ..
    } = parse::parse(Bytes::copy_from_slice(&data)).unwrap();
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

    let parse::Parsed {
        bundle: raw_bundle, ..
    } = parse::parse(Bytes::copy_from_slice(&data)).unwrap();

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
            parse::parse(Bytes::copy_from_slice(&data[..len])).is_err(),
            "parse: truncated at {len} bytes should fail"
        );
    }
}

// A large payload (> chunk_size) that is truncated must not parse as
// complete. The streaming-fallback branch in parse_blocks returns Ok
// even when the buffer is short; parse() must detect and reject it.
#[test]
fn truncated_large_payload() {
    // 50 000-byte payload triggers the streaming fallback whenever the
    // buffer is truncated (the shortfall greatly exceeds chunk_size).
    let (_, full_data) =
        builder::Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
            .with_payload(vec![0xAB_u8; 50_000].as_slice().into())
            .build(creation_timestamp::CreationTimestamp::now())
            .unwrap();

    // Confirm the complete bundle parses successfully.
    assert!(
        parse::parse(Bytes::copy_from_slice(&full_data)).is_ok(),
        "complete large-payload bundle should parse"
    );

    // Truncate to just the primary block + payload block header (well before
    // the payload body ends). parse() must return an error, not Ok.
    let truncated = &full_data[..200];
    assert!(
        parse::parse(Bytes::copy_from_slice(truncated)).is_err(),
        "truncated large-payload bundle must not parse as complete"
    );
}

// A payload byte-string whose declared length pushes the payload block's
// `extent.end` to exactly `usize::MAX`, then truncated so the streaming
// fallback is taken. parse()'s post-finish completeness check must report a
// truncation error, not overflow on `payload_end + 1`.
#[test]
fn crafted_max_extent_payload() {
    let (_, good) = builder::Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
        .with_payload("Hi".as_bytes().into())
        .build(creation_timestamp::CreationTimestamp::now())
        .unwrap();

    // Locate the payload block start by parsing — the primary block's size
    // varies with the creation-timestamp encoding, so it can't be hardcoded.
    // Then graft a fresh CRC-32 payload block header (`86 01 01 04 02 5b` + an
    // 8-byte length) declaring a byte string whose length pushes the block's
    // extent.end to exactly u64::MAX:
    //   extent.end = block_start + header(14) + len + crc_trailer(5)
    // so len = u64::MAX - block_start - 19.
    let block_start = parse::parse(Bytes::copy_from_slice(&good))
        .unwrap()
        .bundle
        .blocks
        .get(&1)
        .expect("payload block")
        .extent
        .start;
    let len = u64::MAX - block_start - 19;

    let mut evil = good[..block_start as usize].to_vec();
    evil.extend_from_slice(&[0x86, 0x01, 0x01, 0x04, 0x02, 0x5b]);
    evil.extend_from_slice(&len.to_be_bytes());

    // Must return a truncation error, not panic on overflow.
    assert!(
        parse::parse(Bytes::copy_from_slice(&evil)).is_err(),
        "crafted max-extent payload must be rejected, not overflow"
    );
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
            parse::parse(Bytes::copy_from_slice(&with_trailing)),
            Err(Error::AdditionalData)
        ),
        "parse with trailing data should return Err(AdditionalData)"
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
        parse::parse(Bytes::copy_from_slice(&tagged)),
        Err(Error::InvalidCBOR(
            hardy_cbor::decode::Error::IncorrectType(..)
        ))
    ));
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
    let parse::Parsed {
        bundle: raw_bundle, ..
    } = parse::parse(Bytes::copy_from_slice(&data)).unwrap();
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
    let parse::Parsed {
        bundle: raw_bundle, ..
    } = parse::parse(Bytes::copy_from_slice(&data)).unwrap();
    assert!(
        raw_bundle.blocks.contains_key(&1),
        "Payload block (block 1) must be present"
    );
    assert!(
        !matches!(raw_bundle.primary.crc_type, crc::CrcType::None),
        "Primary block must have a CRC"
    );
}
