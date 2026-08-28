use bytes::Bytes;
use hardy_bpv7::parse::{self, BundleParser, ParserProgress};

/// Drive the multi-push streaming pipeline over `full`: header chunks through
/// [`BundleParser::push`], then — when an oversized payload takes the
/// `Partial` route — the rest through [`parse::PayloadTail::push`], finishing
/// both state machines. Mirrors the ingress drain loop in `hardy-bpa`.
#[allow(clippy::result_large_err)]
fn drive_streamed(
    full: &[u8],
    parser_chunk: usize,
    push_chunk: usize,
) -> Result<parse::Parsed, hardy_bpv7::Error> {
    let mut parser = BundleParser::new(parser_chunk);
    let mut fed = 0;
    for c in full.chunks(push_chunk) {
        fed += c.len();
        match parser.push(Bytes::copy_from_slice(c))? {
            ParserProgress::NeedMore(_) => {}
            ParserProgress::Ready(whole) => {
                // Bytes left after the bundle completed are trailing data —
                // the one-shot parser sees them in the same buffer and
                // rejects; the streaming caller never pushes them.
                if fed < full.len() {
                    return Err(hardy_bpv7::Error::AdditionalData);
                }
                return parser.finish(whole);
            }
            ParserProgress::Partial { consumed, mut tail } => {
                // Drain the payload tail; a push after the completing one
                // must itself error (`AdditionalData`), so no leftover check
                // is needed on this route.
                let mut complete = false;
                for c in full[fed..].chunks(push_chunk) {
                    complete = tail.push(c)?;
                }
                let _ = complete;
                tail.finish()?;
                return parser.finish(consumed);
            }
        }
    }
    // The input ended mid-bundle.
    Err(hardy_bpv7::Error::InvalidCBOR(
        hardy_cbor::decode::Error::NeedMoreData(1),
    ))
}

/// Differential fuzz of the streaming parser against one-shot [`parse::parse`]:
/// the first two input bytes choose the parser chunk size (small, so any
/// payload beyond a few hundred bytes takes the `Partial`/`PayloadTail` route)
/// and the push granularity; the rest is the bundle. The two pipelines must
/// agree on accept/reject, and on the parsed shape when both accept.
pub fn test_streaming(data: &[u8]) {
    let [seed0, seed1, bundle_bytes @ ..] = data else {
        return;
    };
    let parser_chunk = 16 + *seed0 as usize;
    let push_chunk = 1 + (*seed1 as usize % 64);

    let streamed = drive_streamed(bundle_bytes, parser_chunk, push_chunk);
    let oneshot = parse::parse(Bytes::copy_from_slice(bundle_bytes));

    match (streamed, oneshot) {
        (Ok(s), Ok(o)) => {
            assert_eq!(
                s.bundle.primary.id, o.bundle.primary.id,
                "streamed and one-shot disagree on the bundle id"
            );
            assert_eq!(
                s.bundle.primary.destination, o.bundle.primary.destination,
                "streamed and one-shot disagree on the destination"
            );
            let sorted = |bundle: &hardy_bpv7::Bundle| {
                let mut blocks: Vec<_> = bundle
                    .blocks
                    .iter()
                    .map(|(&n, b)| (n, b.block_type))
                    .collect();
                blocks.sort_unstable();
                blocks
            };
            assert_eq!(
                sorted(&s.bundle),
                sorted(&o.bundle),
                "streamed and one-shot disagree on the block map"
            );
        }
        (Ok(_), Err(e)) => panic!("streamed accepted a bundle one-shot rejects: {e}"),
        (Err(e), Ok(_)) => panic!("streamed rejected a bundle one-shot accepts: {e}"),
        // Both reject: the exact error may legitimately differ by route
        // (e.g. a payload CRC failure surfaces from the tail mid-stream).
        (Err(_), Err(_)) => {}
    }
}

#[cfg(test)]
mod test {
    use super::*;

    // The oracle on a well-formed bundle, through both streaming routes: a
    // small parser chunk against a large payload (Partial/PayloadTail), and
    // truncated input (both pipelines reject).
    #[test]
    fn oracle_agrees_on_builder_bundle() {
        let bundle = hardy_bpv7::builder::Builder::new(
            "ipn:1.0".parse().unwrap(),
            "ipn:2.0".parse().unwrap(),
        )
        .with_payload(vec![0xAB_u8; 50_000].as_slice().into())
        .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
        .unwrap()
        .1;

        // seed0=0 → parser chunk 16 (Partial route); seed1=63 → 64-byte pushes.
        let mut input = vec![0u8, 63u8];
        input.extend_from_slice(&bundle);
        test_streaming(&input);

        // Truncated: drop the outer break + CRC tail.
        test_streaming(&input[..input.len() - 4]);

        // Trailing garbage after the bundle.
        input.push(0xFF);
        test_streaming(&input);
    }
}
