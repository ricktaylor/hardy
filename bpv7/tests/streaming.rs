//! Streaming-parser tests for the `Partial` path: driving
//! [`hardy_bpv7::parse::BundleParser`] segment-by-segment so the payload block
//! is not complete in the buffer when its header parses, then draining the
//! tail through [`PayloadTail`]. These
//! exercise the multi-`push` streaming half of the parser, which the one-shot
//! `parse()` consumers never reach.

use bytes::Bytes;
use hardy_bpv7::{
    Error, builder,
    crc::{self, CrcType},
    creation_timestamp, parse,
    parse::{BundleParser, ParserProgress, PayloadTail},
};
use hex_literal::hex;
// A bundle with a large payload: pushed in pieces, its payload block is
// incomplete when its header parses, so the parser hands back a tail.
fn large_payload_bundle() -> Box<[u8]> {
    builder::Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
        .with_payload(vec![0xAB_u8; 50_000].as_slice().into())
        .build(creation_timestamp::CreationTimestamp::now())
        .unwrap()
        .1
}

// Feed `full` to a fresh parser in `chunk`-byte pushes until it reports
// `Partial`, returning (the parser, consumed-so-far, the tail continuation,
// bytes fed). Callers that don't `finish()` bind the parser to `_`.
fn drive_to_partial(
    full: &[u8],
    chunk: usize,
    parser_chunk: usize,
) -> (BundleParser, Bytes, PayloadTail, usize) {
    let mut parser = BundleParser::new(parser_chunk);
    let mut fed = 0;
    for c in full.chunks(chunk) {
        fed += c.len();
        match parser.push(Bytes::copy_from_slice(c)).unwrap() {
            ParserProgress::NeedMore(_) => {}
            ParserProgress::Partial { consumed, tail } => return (parser, consumed, tail, fed),
            ParserProgress::Ready(_) => {
                panic!("a bundle pushed in pieces must reach Partial, not Ready")
            }
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
    let (parser, consumed, tail, fed) = drive_to_partial(&full, 20, 256);
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
    let (_, _consumed, mut tail, fed) = drive_to_partial(&full, 20, 256);

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
    let (_, _consumed, mut tail, fed) = drive_to_partial(&full, 20, 256);

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
    let (_, _consumed, mut tail, fed) = drive_to_partial(&full, 20, 256);

    // Feed all but the last 4 bytes (CRC tail + outer break never arrive).
    let tail_bytes = &full[fed..];
    let complete = tail.push(&tail_bytes[..tail_bytes.len() - 4]).unwrap();
    assert!(!complete, "incomplete tail must not report complete");
    assert!(
        matches!(
            tail.finish(),
            Err(Error::InvalidCBOR(hardy_cbor::decode::Error::NeedMoreData(
                _
            )))
        ),
        "truncated tail should finish with NeedMoreData"
    );
}

// Bytes pushed after the bundle has completed are trailing data.
#[test]
fn partial_tail_rejects_trailing_data() {
    let full = large_payload_bundle();
    let (_, _consumed, mut tail, fed) = drive_to_partial(&full, 20, 256);

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
    let (_, _consumed, mut tail, fed) = drive_to_partial(&full, 20, 256);

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

// Drive `full` to `Partial`, then push the rest in two pieces split at each of
// the last 8 bytes: every split through the trailer (CRC value, block break,
// outer break) and the end of the body leaves the tail incomplete after the
// first piece and complete, its CRC verified, after the second.
fn assert_tail_settles_split_anywhere_in_its_trailer(full: &[u8]) {
    // The longest trailer, CRC-32 in an indefinite block, is seven bytes
    // (`44 c0 c1 c2 c3 FF FF`); the eighth split falls in the body.
    for from_end in 1..=8 {
        let (_, _consumed, mut tail, fed) = drive_to_partial(full, 20, 256);
        let rest = &full[fed..];
        let split = rest.len() - from_end;
        assert!(
            !tail.push(&rest[..split]).unwrap(),
            "split {from_end} from the end: the first piece leaves it incomplete"
        );
        assert!(
            tail.push(&rest[split..]).unwrap(),
            "split {from_end} from the end: the second piece completes it"
        );
        tail.finish().unwrap();
    }
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
    assert_tail_settles_split_anywhere_in_its_trailer(&full);
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
    assert_tail_settles_split_anywhere_in_its_trailer(&full);
}

// One-shot `parse()` deals only in complete buffers, so a truncated payload
// (which would `Partial` under push) is surfaced as truncation.
#[test]
fn one_shot_rejects_truncated_large_payload() {
    let full = large_payload_bundle();
    let result = parse::parse(Bytes::copy_from_slice(&full[..500]));
    assert!(
        matches!(
            result,
            Err(Error::InvalidCBOR(hardy_cbor::decode::Error::NeedMoreData(
                _
            )))
        ),
        "expected NeedMoreData truncation"
    );
}
// Repro (RUSTSEC-style OOM the ClusterFuzzLite bpa target found): an extension
// block whose data byte string claims a 2^60-byte body must not make the
// parser reserve gigabytes. Only a handful of real bytes are ever fed.
#[test]
fn hostile_claimed_block_length_bounds_reserve() {
    // A real, complete bundle — take its primary block verbatim as a prefix.
    let full = builder::Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
        .with_payload(b"x".as_slice().into())
        .build(creation_timestamp::CreationTimestamp::now())
        .unwrap()
        .1;
    let parsed = {
        let mut p = BundleParser::default();
        let ParserProgress::Ready(whole) = p.push(Bytes::copy_from_slice(&full)).unwrap() else {
            panic!("fixture bundle should parse Ready");
        };
        p.finish(whole).unwrap()
    };
    let prim_end = parsed.bundle.blocks[&0].extent.end as usize;

    // 0x9F outer array + primary block, then a crafted extension block:
    // array(5)[type=7, num=2, flags=0, crc=0, data=bstr(2^60)].
    let mut crafted = full[..prim_end].to_vec();
    crafted.extend_from_slice(&[0x85, 0x07, 0x02, 0x00, 0x00, 0x5B]);
    crafted.extend_from_slice(&(1u64 << 60).to_be_bytes());

    // Must reject or ask for more WITHOUT a giant allocation — reaching this
    // line at all means reserve() did not abort the process.
    let mut parser = BundleParser::new(4096);
    match parser.push(Bytes::from(crafted)) {
        Ok(ParserProgress::NeedMore(_)) | Err(_) => {}
        Ok(_) => panic!("hostile length should not parse as a complete bundle"),
    }
}

// The streamed twin of the 256 MiB pre-payload bound: an extension block
// whose declared body crosses it is refused with the typed error at the
// header, however the bytes are chunked — never `NeedMore` (which would
// invite the CLA to stream a quarter-gigabyte the parser must reject
// anyway).
#[test]
fn oversized_extension_block_rejected_streamed() {
    let full = builder::Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
        .with_payload(b"x".as_slice().into())
        .build(creation_timestamp::CreationTimestamp::now())
        .unwrap()
        .1;
    let parsed = {
        let mut p = BundleParser::default();
        let ParserProgress::Ready(whole) = p.push(Bytes::copy_from_slice(&full)).unwrap() else {
            panic!("fixture bundle should parse Ready");
        };
        p.finish(whole).unwrap()
    };
    let prim_end = parsed.bundle.blocks[&0].extent.end;

    // array(5)[type=7, num=2, flags=0, crc=none, data=bstr(len)] with the
    // canonical 4-byte length head (256 MiB fits u32): the body starts 10
    // bytes after the block and the extent ends where the body does (no CRC
    // trailer).
    let len: u64 = 256 * 1024 * 1024;
    let mut crafted = full[..prim_end as usize].to_vec();
    crafted.extend_from_slice(&[0x85, 0x07, 0x02, 0x00, 0x00, 0x5A]);
    crafted.extend_from_slice(&u32::try_from(len).unwrap().to_be_bytes());

    // Push in small chunks so the block header itself crosses pushes; the
    // typed rejection must still surface once the header is whole.
    let mut parser = BundleParser::new(4096);
    let mut result = None;
    for chunk in crafted.chunks(7) {
        match parser.push(Bytes::copy_from_slice(chunk)) {
            Ok(ParserProgress::NeedMore(_)) => continue,
            other => {
                result = Some(other);
                break;
            }
        }
    }
    assert!(
        matches!(
            result,
            Some(Err(Error::ExtensionBlocksTooLarge(end))) if end == prim_end + 10 + len
        ),
        "streamed oversized extension block must fail with the typed bound error"
    );
}

// Push `full` one byte at a time: every possible chunk boundary lands inside
// some field, so a `NeedMoreData` surfacing anywhere in the field-error chain
// that is mis-read as a structural reject (instead of being buffered as
// `NeedMore`) fails here. The parser needs more through the header region,
// goes `Partial` the byte the payload block's header completes (the payload
// has not arrived), and the tail, fed the rest a byte at a time, completes at
// the last byte. Returns the header index.
fn push_byte_by_byte(full: &[u8]) -> parse::Parsed {
    let mut parser = BundleParser::default();
    for (i, b) in full.iter().enumerate() {
        match parser
            .push(Bytes::copy_from_slice(&[*b]))
            .unwrap_or_else(|e| panic!("push of byte {i} must not hard-fail: {e:?}"))
        {
            ParserProgress::NeedMore(_) => {}
            ParserProgress::Partial { consumed, mut tail } => {
                assert_eq!(consumed.as_ref(), &full[..=i], "consumed is a prefix");
                let rest = &full[i + 1..];
                for (j, b) in rest.iter().enumerate() {
                    let complete = tail
                        .push(&[*b])
                        .unwrap_or_else(|e| panic!("tail byte {j} must not fail: {e:?}"));
                    assert_eq!(complete, j + 1 == rest.len(), "tail byte {j}");
                }
                tail.finish().expect("the tail completes at the last byte");
                return parser.finish(consumed).expect("the headers parse");
            }
            ParserProgress::Ready(_) => {
                panic!("a bundle whose payload has not arrived is not Ready")
            }
        }
    }
    panic!("the parser never reached the payload block");
}

#[test]
fn byte_by_byte_push_streams_the_payload() {
    let full = builder::Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
        .with_payload(b"tiny".as_slice().into())
        .build(creation_timestamp::CreationTimestamp::now())
        .unwrap()
        .1;

    let parsed = push_byte_by_byte(&full);
    assert_eq!(parsed.bundle.primary.id.source, "ipn:1.0".parse().unwrap());
}

// Regression: a valid `#6.24`-tagged block body whose `D8 18` head straddles
// a chunk boundary. When a push ends exactly on the lone `0xD8`, the
// block-data tag guard must report the shortfall as `NeedMore` — not
// misclassify the half-arrived head as a permanent `NotCanonical` reject and
// drop an otherwise-valid bundle. Byte-by-byte pushes force every boundary,
// including that one.
#[test]
fn byte_by_byte_push_of_tag24_block_data_streams_the_payload() {
    // A hand-crafted bundle (no CRCs) whose payload data is
    // #6.24(bstr "HELLO") — Hardy's own encoder never emits the tag, so
    // this is receive-side interop input by construction.
    let full = hex!(
        "9f88070000820282010282028202018202820201820018281a000f4240"
        "8501010000d8184548454c4c4f"
        "ff"
    );

    let parsed = push_byte_by_byte(&full);
    let payload = parsed.bundle.blocks.get(&1).expect("payload block");
    assert_eq!(payload.payload(&full).expect("payload in bundle"), b"HELLO");
}

// A bundle whose payload fits inside one parser chunk.
fn small_payload_bundle() -> Box<[u8]> {
    builder::Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
        .with_payload(vec![0xCD_u8; 1000].as_slice().into())
        .build(creation_timestamp::CreationTimestamp::now())
        .unwrap()
        .1
}

// A push of `full` short by `short_by` bytes: `Partial`, with the parsed
// headers in `consumed` and a tail that cannot finish, its `remaining` the
// exact shortfall.
fn assert_short_bundle_is_partial(full: &[u8], short_by: usize) {
    let short = &full[..full.len() - short_by];
    let mut parser = BundleParser::default();
    let ParserProgress::Partial { consumed, tail } = parser
        .push(Bytes::copy_from_slice(short))
        .unwrap_or_else(|e| panic!("short by {short_by}: push must not fail: {e:?}"))
    else {
        panic!("short by {short_by}: a bundle short of its end must be Partial");
    };
    assert_eq!(consumed.as_ref(), short, "short by {short_by}: consumed");
    assert_eq!(
        tail.remaining(),
        short_by as u64,
        "short by {short_by}: remaining is the shortfall"
    );
    assert!(
        matches!(
            tail.finish(),
            Err(Error::InvalidCBOR(hardy_cbor::decode::Error::NeedMoreData(n))) if n == short_by
        ),
        "short by {short_by}: the tail cannot finish"
    );
    let parsed = parser
        .finish(consumed)
        .unwrap_or_else(|e| panic!("short by {short_by}: the headers parse: {e:?}"));
    assert_eq!(parsed.bundle.primary.id.source, "ipn:1.0".parse().unwrap());
}

// Once the payload block's header has parsed, the parser never waits: a
// bundle short of its end is `Partial` however short — by less than one
// parser chunk (100), cut exactly at the body's end (6: the CRC-32 trailer
// and the outer break), inside the CRC value (3), or missing only its outer
// break (1).
#[test]
fn a_short_bundle_is_partial() {
    let full = small_payload_bundle();
    for short_by in [100, 6, 3, 1] {
        assert_short_bundle_is_partial(&full, short_by);
    }
}

// The same for the trailer shapes the Builder never emits: an empty payload
// with no CRC (its block ends where its body starts) missing its outer break,
// and an indefinite-length payload block short at either break or inside its
// CRC value.
#[test]
fn a_short_crafted_bundle_is_partial() {
    let empty = craft_bundle(CrcType::None, false, &[]);
    assert!(parse::parse(Bytes::copy_from_slice(&empty)).is_ok());
    assert_short_bundle_is_partial(&empty, 1);

    let indefinite = craft_bundle(CrcType::CRC32_CASTAGNOLI, true, &[0xEF; 1000]);
    assert!(parse::parse(Bytes::copy_from_slice(&indefinite)).is_ok());
    for short_by in [1, 2, 4] {
        assert_short_bundle_is_partial(&indefinite, short_by);
    }
}

// Short of the payload block's header, the parser needs more: the header
// region itself is incomplete, so there are no parsed headers to hand back.
#[test]
fn a_short_header_region_needs_more() {
    let full = small_payload_bundle();
    let payload_start = parse::parse(Bytes::copy_from_slice(&full))
        .unwrap()
        .bundle
        .blocks
        .get(&1)
        .expect("payload block")
        .extent
        .start as usize;
    // Only the payload block's array head arrived.
    let short = &full[..payload_start + 1];
    assert!(matches!(
        BundleParser::default().push(Bytes::copy_from_slice(short)),
        Ok(ParserProgress::NeedMore(_))
    ));
}

// A whole bundle pushed at once whose payload trailer fails: the parser hands
// back the parsed headers with a tail that failed, and the tail's `finish`
// returns the failure.
fn failed_tail(bytes: &[u8]) -> Error {
    let mut parser = BundleParser::default();
    let Ok(ParserProgress::Partial { consumed, tail }) = parser.push(Bytes::copy_from_slice(bytes))
    else {
        panic!("a payload trailer failure must be Partial");
    };
    parser.finish(consumed).expect("the headers parse");
    tail.finish().expect_err("the tail failed")
}

// A payload trailer that fails in the buffer is the tail's verdict, as it
// would be in a later push: a CRC mismatch, bytes after the outer break, or a
// malformed outer break. The CRC-32 trailer is `44 c0 c1 c2 c3 FF`.
#[test]
fn a_trailer_failure_in_the_buffer_is_the_tails() {
    let full = small_payload_bundle();

    let mut bad_crc = full.to_vec();
    let last_crc_byte = bad_crc.len() - 2;
    bad_crc[last_crc_byte] ^= 0xFF;
    assert!(matches!(
        failed_tail(&bad_crc),
        Error::InvalidCrc(crc::Error::IncorrectCrc)
    ));

    let mut trailing = full.to_vec();
    trailing.push(0x00);
    assert!(matches!(failed_tail(&trailing), Error::AdditionalData));

    let mut bad_break = full.to_vec();
    let outer_break = bad_break.len() - 1;
    bad_break[outer_break] = 0x00;
    assert!(matches!(failed_tail(&bad_break), Error::NotCanonical));

    // The failure is reported by the tail's next push too.
    let Ok(ParserProgress::Partial { mut tail, .. }) =
        BundleParser::default().push(Bytes::copy_from_slice(&bad_crc))
    else {
        panic!("a payload trailer failure must be Partial");
    };
    assert!(matches!(
        tail.push(&[]),
        Err(Error::InvalidCrc(crc::Error::IncorrectCrc))
    ));
}

// Payloads of zero and one byte, pushed a byte at a time, stream to
// completion: the tail starts at the trailer, or one byte before it.
#[test]
fn byte_by_byte_push_streams_tiny_payloads() {
    for payload in [&b""[..], b"x"] {
        let full = builder::Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
            .with_payload(payload.into())
            .build(creation_timestamp::CreationTimestamp::now())
            .unwrap()
            .1;
        let parsed = push_byte_by_byte(&full);
        assert_eq!(parsed.bundle.blocks[&1].payload(&full), Some(payload));
    }
}

// A block spliced after the payload block fails the tail, which expects the
// outer break: in the buffer, and on the streaming route alike.
#[test]
fn a_block_after_the_payload_fails_the_tail() {
    let full = small_payload_bundle();
    // array(5)[type 7, number 2, flags 0, CRC none, empty bstr] before the
    // outer break.
    let mut spliced = full[..full.len() - 1].to_vec();
    spliced.extend_from_slice(&[0x85, 0x07, 0x02, 0x00, 0x00, 0x40, 0xFF]);

    assert!(matches!(failed_tail(&spliced), Error::NotCanonical));

    let (_, _consumed, mut tail, fed) = drive_to_partial(&spliced, 20, 256);
    assert!(matches!(
        tail.push(&spliced[fed..]),
        Err(Error::NotCanonical)
    ));
}
