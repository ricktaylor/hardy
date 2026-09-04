//! Integration tests for primary-block parsing/validation via the public
//! `hardy_bpv7` API (Builder → bytes → parse).

use bytes::Bytes;
use hardy_bpv7::{
    Error, builder, bundle, crc, creation_timestamp, dtn_time, eid, parse,
    primary_block::PrimaryBlock,
};
// Aliased: `decode::Error` collides with the bpv7 `Error` and
// `encode::Bytes` with `bytes::Bytes` imported above.
use hardy_cbor::{
    decode::{Error as CborError, FromCbor},
    encode::{Bytes as CborBytes, emit_array},
};

fn build_bundle_with_crc(crc_type: crc::CrcType) -> Box<[u8]> {
    builder::Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
        .with_crc_type(crc_type)
        .with_payload("Test".as_bytes().into())
        .build(creation_timestamp::CreationTimestamp::now())
        .unwrap()
        .1
}

// LLR 1.1.21: Parser must parse and validate all CRC values
// (CRC validation lives at the parse layer; no keys needed.)
#[test]
fn valid_crc() {
    // CRC-32 (default) — valid bundle should parse
    let data = build_bundle_with_crc(crc::CrcType::CRC32_CASTAGNOLI);
    assert!(parse::parse(Bytes::copy_from_slice(&data)).is_ok());

    // CRC-16 — valid bundle should parse
    let data = build_bundle_with_crc(crc::CrcType::CRC16_X25);
    assert!(parse::parse(Bytes::copy_from_slice(&data)).is_ok());
}

#[test]
fn invalid_crc() {
    for crc_type in [crc::CrcType::CRC16_X25, crc::CrcType::CRC32_CASTAGNOLI] {
        let data = build_bundle_with_crc(crc_type);

        // Locate the primary block so the corruption targets the stored CRC
        // value itself, not CBOR structure: the CRC value is the final field
        // of the primary block, so the last byte of the block extent is
        // inside it.
        let parsed = parse::parse(Bytes::copy_from_slice(&data)).expect("valid bundle must parse");
        let primary_extent = parsed
            .bundle
            .blocks
            .get(&0)
            .expect("primary block missing")
            .extent
            .clone();
        let crc_last = usize::try_from(primary_extent.end).unwrap() - 1;

        let mut data = data.to_vec();
        data[crc_last] ^= 0x01;

        let Err(Error::InvalidField {
            field: "primary block",
            source,
        }) = parse::parse(Bytes::from(data))
        else {
            panic!("corrupted {crc_type:?} must fail as a primary-block field error");
        };
        let Some(Error::InvalidField {
            field: "CRC value",
            source,
        }) = source.downcast_ref::<Error>()
        else {
            panic!("the primary-block failure must name the CRC value, got {source:?}");
        };
        assert!(
            matches!(
                source.downcast_ref::<Error>(),
                Some(Error::InvalidCrc(crc::Error::IncorrectCrc))
            ),
            "expected IncorrectCrc for {crc_type:?}, got: {source:?}"
        );
    }
}

// LLR 1.1.22 (Parser must support all CRC types — CRC-16 and CRC-32)
// is covered by `valid_crc` above, which exercises both types via the
// structural parser.

// LLR 1.1.15: Parser must indicate that the Primary Block is valid
// (Primary-block validation lives at the parse layer; no keys needed.)
#[test]
fn primary_block_validation() {
    // Valid bundle parses successfully
    let data = build_bundle_with_crc(crc::CrcType::CRC32_CASTAGNOLI);
    let parse::Parsed { data, bundle, .. } = parse::parse(Bytes::copy_from_slice(&data)).unwrap();
    assert_eq!(bundle.primary.id.source, "ipn:1.0".parse().unwrap());

    // Bundle with version != 7 should fail
    // The primary block starts at byte 1 (after 0x9F outer array).
    // The primary block is a CBOR array, first element is version (7).
    // Find and corrupt the version field.
    let mut bad_version = data.to_vec();
    // The version 7 is encoded as CBOR unsigned int 7 = 0x07
    // It appears after the primary block array header
    // Primary block: 0x89 (array of 9) then 0x07 (version 7)
    let pos = bad_version
        .windows(2)
        .position(|w| w == [0x89, 0x07])
        .expect("version byte pattern [0x89, 0x07] not found — test fixture needs updating");
    bad_version[pos + 1] = 0x06; // change version to 6
    let result = parse::parse(Bytes::copy_from_slice(&bad_version));
    // Downcast the source: rejection must be for the version itself, not some
    // other primary-block failure the byte edit could provoke (e.g. the CRC).
    let Err(Error::InvalidField {
        field: "primary block",
        source,
    }) = result
    else {
        panic!("version 6 must fail as an InvalidField primary-block error");
    };
    assert!(
        matches!(
            source.downcast_ref::<Error>(),
            Some(Error::InvalidVersion(6))
        ),
        "the primary-block failure must be InvalidVersion(6), got {source}"
    );
}

// Hand-encode a primary block without CRC: an array of 8 fields, plus
// fragment offset and total ADU length when `fragment_fields` supplies
// them. Field order per RFC 9171 §4.3.1: version, flags, crc_type,
// destination, source, report_to, creation timestamp, lifetime[, offset,
// total].
fn emit_primary(flags: u64, fragment_fields: Option<(u64, u64)>) -> Vec<u8> {
    emit_array(Some(if fragment_fields.is_some() { 10 } else { 8 }), |a| {
        a.emit(&7u64); // version
        a.emit(&flags);
        a.emit(&0u64); // CRC type: none
        a.emit(&"ipn:2.0".parse::<eid::Eid>().unwrap()); // destination
        a.emit(&"ipn:1.0".parse::<eid::Eid>().unwrap()); // source
        a.emit(&"ipn:1.0".parse::<eid::Eid>().unwrap()); // report-to
        a.emit(&creation_timestamp::CreationTimestamp::from_parts(
            Some(dtn_time::DtnTime::new(820_000_000_000)),
            1,
        ));
        a.emit(&86_400_000u64); // lifetime (ms)
        if let Some((offset, total_adu_length)) = fragment_fields {
            a.emit(&offset);
            a.emit(&total_adu_length);
        }
    })
}

// RFC 9171 §4.3.1: fragment offset and total ADU length are present exactly
// when the is_fragment flag (bit 0) is set, and a fragment cannot start
// beyond the end of the original ADU.
#[test]
fn fragment_primary_block_parsing() {
    // Interior fragment: offset zero.
    let data = emit_primary(0x01, Some((0, 5000)));
    let (block, _, _) = PrimaryBlock::from_cbor(&data).expect("should parse");
    assert!(block.flags.is_fragment);
    assert_eq!(
        block.id.fragment_info,
        Some(bundle::FragmentInfo {
            offset: 0,
            total_adu_length: 5000
        })
    );

    // Boundary: offset == total ADU length is legal (empty final fragment).
    let data = emit_primary(0x01, Some((5000, 5000)));
    let (block, _, _) = PrimaryBlock::from_cbor(&data).expect("should parse");
    assert_eq!(
        block.id.fragment_info,
        Some(bundle::FragmentInfo {
            offset: 5000,
            total_adu_length: 5000
        })
    );

    // offset > total ADU length is rejected during the primary-block parse.
    let data = emit_primary(0x01, Some((5001, 5000)));
    assert!(
        matches!(
            PrimaryBlock::from_cbor(&data),
            Err(Error::InvalidFragmentInfo(5001, 5000))
        ),
        "offset 5001 > total 5000 should be InvalidFragmentInfo, got: {:?}",
        PrimaryBlock::from_cbor(&data)
    );
}

// A non-fragment primary block carrying the two extra fragment fields is
// structurally invalid: with is_fragment clear the ninth element can only
// be the CRC value, and an unsigned integer there fails that field's parse.
#[test]
fn non_fragment_with_fragment_fields_rejected() {
    let data = emit_primary(0x00, Some((40, 5000)));
    let Err(Error::InvalidField {
        field: "CRC value",
        source,
    }) = PrimaryBlock::from_cbor(&data)
    else {
        panic!("a non-fragment primary block with 10 fields must fail the CRC-value parse");
    };
    assert!(
        matches!(
            source.downcast_ref::<Error>(),
            Some(Error::InvalidCBOR(CborError::IncorrectType(
                "Definite-length Byte String",
                _
            )))
        ),
        "expected an incorrect-type CRC value, got {source:?}"
    );
}

// Through the full bundle parse an invalid fragment offset surfaces as an
// InvalidField error, under the primary block, wrapping InvalidFragmentInfo.
#[test]
fn fragment_bundle_parsing() {
    fn make_bundle(offset: u64, total: u64) -> Vec<u8> {
        let mut data = vec![0x9Fu8]; // indefinite-length bundle array
        data.extend_from_slice(&emit_primary(0x01, Some((offset, total))));
        // Payload block [1, 1, flags=0, crc_type=0, data]
        data.extend_from_slice(&emit_array(Some(5), |a| {
            a.emit(&1u64);
            a.emit(&1u64);
            a.emit(&0u64);
            a.emit(&0u64);
            a.emit(&CborBytes(b"Hi"));
        }));
        data.push(0xFF); // break
        data
    }

    // A valid fragment parses and carries its fragment info in the id.
    let parsed = parse::parse(Bytes::copy_from_slice(&make_bundle(40, 5000))).unwrap();
    assert!(parsed.bundle.primary.flags.is_fragment);
    assert_eq!(
        parsed.bundle.primary.id.fragment_info,
        Some(bundle::FragmentInfo {
            offset: 40,
            total_adu_length: 5000
        })
    );

    // An invalid offset is rejected as a primary-block field error.
    let result = parse::parse(Bytes::copy_from_slice(&make_bundle(5001, 5000)));
    let Err(Error::InvalidField {
        field: "primary block",
        source,
    }) = result
    else {
        panic!("invalid fragment offset should fail as a primary-block error");
    };
    assert!(
        matches!(
            source.downcast_ref::<Error>(),
            Some(Error::InvalidFragmentInfo(5001, 5000))
        ),
        "expected InvalidFragmentInfo(5001, 5000), got: {source:?}"
    );
}
