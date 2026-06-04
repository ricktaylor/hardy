//! Streaming-parser tests for the oversized-payload `Partial` path: driving
//! [`hardy_bpv7::parse::BundleParser`] segment-by-segment so the payload body
//! exceeds the buffer, then draining the tail through [`PayloadTail`]. These
//! exercise the multi-`push` streaming half of the parser, which the one-shot
//! `parse()` consumers never reach.

use bytes::Bytes;
use hardy_bpv7::parse::{BundleParser, ParserProgress, PayloadTail};
use hardy_bpv7::{
    Error, builder,
    crc::{self, CrcType},
    creation_timestamp, parse,
};

// A bundle with a payload far larger than any sane parser chunk size, so the
// streaming fallback fires once the payload header is passed.
fn large_payload_bundle() -> Box<[u8]> {
    builder::Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
        .with_payload(vec![0xAB_u8; 50_000].as_slice().into())
        .build(creation_timestamp::CreationTimestamp::now())
        .unwrap()
        .1
}

// Feed `full` to a fresh parser in `chunk`-byte pushes until it reports
// `Partial`, returning (consumed-so-far, the tail continuation, bytes fed).
fn drive_to_partial(full: &[u8], chunk: usize, parser_chunk: usize) -> (Bytes, PayloadTail, usize) {
    let mut parser = BundleParser::new(parser_chunk);
    let mut fed = 0;
    for c in full.chunks(chunk) {
        fed += c.len();
        match parser.push(Bytes::copy_from_slice(c)).unwrap() {
            ParserProgress::NeedMore(_) => {}
            ParserProgress::Partial { consumed, tail } => return (consumed, tail, fed),
            ParserProgress::Ready(_) => panic!("oversized payload must not parse as Ready"),
        }
    }
    panic!("parser never reached Partial");
}

// Push reports Partial only after the headers are parsed; `consumed` is a true
// prefix, `remaining` is exact, and `finish` yields a correct header index with
// an over-claiming payload extent.
#[test]
fn large_payload_partial_then_finish() {
    let full = large_payload_bundle();

    // 20-byte chunks so the primary block alone spans several pushes (exercises
    // the NeedMore caching + freeze path), with a 256-byte parser chunk size.
    let mut parser = BundleParser::new(256);
    let mut fed = 0;
    let mut reached = None;
    for c in full.chunks(20) {
        fed += c.len();
        match parser.push(Bytes::copy_from_slice(c)).unwrap() {
            ParserProgress::NeedMore(_) => {}
            ParserProgress::Partial { consumed, tail } => {
                reached = Some((consumed, tail, fed));
                break;
            }
            ParserProgress::Ready(_) => panic!("oversized payload must not parse as Ready"),
        }
    }

    let (consumed, tail, fed) = reached.expect("parser should reach Partial");
    assert_eq!(consumed.len(), fed, "consumed is everything pushed so far");
    assert_eq!(
        consumed.as_ref(),
        &full[..fed],
        "consumed is a prefix of the bundle"
    );
    assert_eq!(
        tail.remaining(),
        full.len() as u64 - fed as u64,
        "remaining runs from consumed to the outer break"
    );

    let parsed = parser.finish(consumed).unwrap();
    assert_eq!(parsed.bundle.primary.id.source, "ipn:1.0".parse().unwrap());
    assert_eq!(
        parsed.bundle.primary.destination,
        "ipn:2.0".parse().unwrap()
    );
    assert!(
        parsed.bundle.blocks.contains_key(&0),
        "primary block present"
    );
    let payload = parsed.bundle.blocks.get(&1).expect("payload block present");
    assert_eq!(
        payload.extent.end,
        full.len() as u64 - 1,
        "payload extent claims the full, not-yet-resident block"
    );
}

// Draining the rest of the bundle through the tail completes it and verifies
// the (Builder-computed, CRC-32) payload CRC.
#[test]
fn partial_tail_drains_and_verifies_crc() {
    let full = large_payload_bundle();
    let (_consumed, mut tail, fed) = drive_to_partial(&full, 20, 256);

    let mut complete = false;
    for c in full[fed..].chunks(37) {
        assert!(!complete, "tail reported complete before the last chunk");
        complete = tail.push(c).unwrap();
    }
    assert!(
        complete,
        "tail should complete once the outer break is consumed"
    );
    tail.finish().unwrap();
}

// A flipped body byte in the streamed tail fails the CRC.
#[test]
fn partial_tail_detects_crc_corruption() {
    let full = large_payload_bundle();
    let (_consumed, mut tail, fed) = drive_to_partial(&full, 20, 256);

    // Byte 0 of the tail is well inside the payload body.
    let mut corrupt = full[fed..].to_vec();
    corrupt[0] ^= 0xFF;

    let err = tail.push(&corrupt).unwrap_err();
    assert!(
        matches!(err, Error::InvalidCrc(crc::Error::IncorrectCrc)),
        "expected IncorrectCrc, got {err:?}"
    );
}

// A tail that stops before the outer break is a truncated bundle.
#[test]
fn partial_tail_detects_truncation() {
    let full = large_payload_bundle();
    let (_consumed, mut tail, fed) = drive_to_partial(&full, 20, 256);

    // Feed all but the last 4 bytes (CRC tail + outer break never arrive).
    let tail_bytes = &full[fed..];
    let complete = tail.push(&tail_bytes[..tail_bytes.len() - 4]).unwrap();
    assert!(!complete, "incomplete tail must not report complete");
    assert!(
        matches!(tail.finish(), Err(Error::InvalidCBOR(_))),
        "truncated tail should finish with NeedMoreData"
    );
}

// Bytes pushed after the bundle has completed are trailing data.
#[test]
fn partial_tail_rejects_trailing_data() {
    let full = large_payload_bundle();
    let (_consumed, mut tail, fed) = drive_to_partial(&full, 20, 256);

    assert!(tail.push(&full[fed..]).unwrap(), "tail should complete");
    let err = tail.push(&[0xFF]).unwrap_err();
    assert!(
        matches!(err, Error::AdditionalData),
        "expected AdditionalData, got {err:?}"
    );
}

// Smuggling guard: bytes appended after the outer break *within the same push*
// that completes the bundle must be rejected wholesale — a sender cannot tack a
// second bundle / injected content onto the terminating segment.
#[test]
fn partial_tail_rejects_trailing_in_completing_push() {
    let full = large_payload_bundle();
    let (_consumed, mut tail, fed) = drive_to_partial(&full, 20, 256);

    let mut tail_plus_smuggled = full[fed..].to_vec();
    tail_plus_smuggled.push(0xAB); // one extra byte past the outer break
    let err = tail.push(&tail_plus_smuggled).unwrap_err();
    assert!(
        matches!(err, Error::AdditionalData),
        "trailing bytes in the completing push must be rejected, got {err:?}"
    );
}

// Hand-craft a payload block in a given (crc_type, indefinite) shape with a
// `body`-byte payload, returning the whole bundle: outer array + a real primary
// block (lifted from a Builder bundle) + the crafted payload block + outer
// break. The CRC, when present, is computed exactly as the parser does.
fn craft_bundle(crc_type: CrcType, indefinite: bool, body: &[u8]) -> Vec<u8> {
    // Reuse a real `0x9F` + primary block from a minimal Builder bundle. Its
    // length varies with the creation-timestamp encoding, so locate the payload
    // block start by parsing rather than hardcoding an offset.
    let minimal = builder::Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
        .with_payload(b"x".as_slice().into())
        .build(creation_timestamp::CreationTimestamp::now())
        .unwrap()
        .1;
    let prefix_len = parse::parse(Bytes::copy_from_slice(&minimal))
        .unwrap()
        .bundle
        .blocks
        .get(&1)
        .expect("payload block")
        .extent
        .start as usize;
    let mut out = minimal[..prefix_len].to_vec();

    // Payload block: array head, then type=1, number=1, flags=0, crc_type.
    let mut block = vec![if indefinite {
        0x9F
    } else if matches!(crc_type, CrcType::None) {
        0x85
    } else {
        0x86
    }];
    block.extend_from_slice(&[0x01, 0x01, 0x00, u64::from(crc_type) as u8]);
    // Body as a definite-length byte string with a canonical (shortest) head.
    let len = body.len();
    if len < 24 {
        block.push(0x40 | len as u8);
    } else if len < 0x100 {
        block.extend_from_slice(&[0x58, len as u8]);
    } else if len < 0x1_0000 {
        block.push(0x59);
        block.extend_from_slice(&(len as u16).to_be_bytes());
    } else if len < 0x1_0000_0000 {
        block.push(0x5A);
        block.extend_from_slice(&(len as u32).to_be_bytes());
    } else {
        block.push(0x5B);
        block.extend_from_slice(&(len as u64).to_be_bytes());
    }
    block.extend_from_slice(body);

    if !matches!(crc_type, CrcType::None) {
        let head = match crc_type {
            CrcType::CRC16_X25 => 0x42,
            CrcType::CRC32_CASTAGNOLI => 0x44,
            _ => unreachable!(),
        };
        // CRC over: block-so-far + head + zeroed value + (block break if indef).
        let mut digest = crc::Digest::new(crc_type).unwrap();
        digest.push(&block);
        digest.push(&[head]);
        digest.push_zeros();
        if indefinite {
            digest.push(&[0xFF]);
        }
        let value = digest.finalize();
        block.push(head);
        block.extend_from_slice(&value);
    }
    if indefinite {
        block.push(0xFF); // block-level break
    }

    out.extend_from_slice(&block);
    out.push(0xFF); // outer break
    out
}

// No-CRC, indefinite-length payload block: the tail's no-digest + block-break
// path. The crafted bundle is itself valid (one-shot parse accepts it).
#[test]
fn crc_none_indefinite_payload() {
    let full = craft_bundle(CrcType::None, true, &vec![0xAB_u8; 50_000]);
    assert!(
        parse::parse(Bytes::copy_from_slice(&full)).is_ok(),
        "craft is a valid bundle"
    );

    let (_consumed, mut tail, fed) = drive_to_partial(&full, 20, 256);
    assert!(tail.push(&full[fed..]).unwrap(), "tail should complete");
    tail.finish().unwrap();
}

// CRC-32, indefinite-length payload block: the tail feeds the block-level break
// into the digest before verifying.
#[test]
fn crc32_indefinite_payload() {
    let full = craft_bundle(CrcType::CRC32_CASTAGNOLI, true, &vec![0xCD_u8; 50_000]);
    assert!(
        parse::parse(Bytes::copy_from_slice(&full)).is_ok(),
        "craft is a valid bundle"
    );

    let (_consumed, mut tail, fed) = drive_to_partial(&full, 20, 256);
    assert!(
        tail.push(&full[fed..]).unwrap(),
        "tail should complete + verify CRC"
    );
    tail.finish().unwrap();
}

// One-shot `parse()` deals only in complete buffers, so a truncated oversized
// payload (which would `Partial` under push) is surfaced as truncation.
#[test]
fn one_shot_rejects_truncated_large_payload() {
    let full = large_payload_bundle();
    let result = parse::parse(Bytes::copy_from_slice(&full[..500]));
    assert!(
        matches!(result, Err(Error::InvalidCBOR(_))),
        "expected NeedMoreData truncation"
    );
}
