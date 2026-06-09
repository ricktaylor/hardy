//! Integration tests for primary-block parsing/validation via the public
//! `hardy_bpv7` API (Builder → bytes → parse).

use hardy_bpv7::{builder, crc, creation_timestamp, parse};

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
    assert!(parse::parse(bytes::Bytes::copy_from_slice(&data)).is_ok());

    // CRC-16 — valid bundle should parse
    let data = build_bundle_with_crc(crc::CrcType::CRC16_X25);
    assert!(parse::parse(bytes::Bytes::copy_from_slice(&data)).is_ok());
}

#[test]
fn invalid_crc() {
    let mut data = build_bundle_with_crc(crc::CrcType::CRC32_CASTAGNOLI).to_vec();

    // Corrupt a byte in the primary block (flip a bit in the middle)
    // The primary block starts at byte 1 (after 0x9F)
    let corrupt_pos = data.len() / 3;
    data[corrupt_pos] ^= 0x01;

    let result = parse::parse(bytes::Bytes::copy_from_slice(&data));
    assert!(result.is_err(), "Corrupted CRC should fail to parse");
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
    let parse::Parsed { data, bundle, .. } =
        parse::parse(bytes::Bytes::copy_from_slice(&data)).unwrap();
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
    let result = parse::parse(bytes::Bytes::copy_from_slice(&bad_version));
    assert!(
        result.is_err(),
        "Bundle with version 6 should fail to parse"
    );
}
