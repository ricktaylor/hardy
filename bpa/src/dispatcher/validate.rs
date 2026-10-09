//! The validating decorator for a received bundle's segment stream.
//!
//! [`parse_headers`](crate::bundle::parse::parse_headers) hands back the
//! resident prefix, a synchronous [`PayloadTail`] continuation, and —
//! begun in its keyed pass via
//! [`begin_payload_verification`](hardy_bpv7::checks::begin_payload_verification)
//! — one incremental [`bib::Verifier`] per deferred payload BIB. An input
//! door (CLA ingress today; the raw originate door when it streams) marries
//! those to the arrival's segment stream through a [`ValidatingReceiver`],
//! then drains it through `Store::save_stream`: the validation is invisible
//! downstream, surfacing only as a pull that fails when the bytes are bad,
//! with the categorised verdict settled by `finish` once the drain returns —
//! and mapped by each door to its own surface (status reports at ingress, a
//! `services::Error` at originate).

use hardy_async::async_trait;
use hardy_bpv7::{bpsec::bib, parse::PayloadTail, status_report::ReasonCode};
use thiserror::Error;

use crate::{
    Bytes,
    bundle::parse,
    cla::Segment,
    stream::{Receiver, RecvError},
};

/// Why a [`ValidatingReceiver`] rejected the payload's bytes, whether they
/// were resident at the header pass or drained.
#[derive(Debug, Error)]
pub enum ValidationFailure {
    /// The stream ended before its `Final`, mid-bundle or after the outer
    /// break: the producer went away, or the drain was cancelled. A resend
    /// may complete it, so the transfer is refused (the CLA withholds its
    /// acknowledgement).
    #[error("the stream ended before its final segment")]
    Truncated,
    /// The payload's bytes were structurally invalid — payload CRC mismatch,
    /// a malformed trailer, bytes past the outer break, or a payload the
    /// stream's `Final` cut short. The transfer is complete but the bundle
    /// unacceptable: accepted and dropped, never refused.
    #[error("invalid payload bytes: {0}")]
    Invalid(hardy_bpv7::Error),
    /// A deferred payload BIB failed integrity over the streamed body
    /// (RFC 9172 §5.1.1). Names the BIB block that made the claim; the
    /// bundle is accepted and dropped.
    #[error("deferred payload BIB {bib} failed integrity over the streamed body")]
    IntegrityFailed { bib: u64 },
}

impl ValidationFailure {
    /// The status-report reason this failure raises, so ingress's settle
    /// arm reports the drop like any other parsing failure (the combined RFC
    /// 9171 §5.6/§5.10 reception + deletion status report, per the bundle's
    /// flags). `None` for [`Truncated`](Self::Truncated): a refused transfer
    /// is never reported — the peer retains custody and may resend.
    pub fn reason_code(&self) -> Option<ReasonCode> {
        match self {
            Self::Truncated => None,
            Self::Invalid(error) => Some(parse::status_report_reason_for(error)),
            Self::IntegrityFailed { .. } => Some(ReasonCode::FailedSecurityOperation),
        }
    }
}

// Where a drain stands. Only an open drain pulls.
enum Progress {
    // The stream has not yet delivered its `Final`.
    Open,
    // The stream delivered its `Final`, and every segment absorbed cleanly.
    Final,
    // A pull failed: the bytes were bad, or the producer went away.
    Failed(ValidationFailure),
}

// A [`Receiver<Segment>`] decorator that validates a received bundle's
// payload tail as it streams through.
//
// `parse_headers` hands back the resident prefix, a synchronous
// [`PayloadTail`] continuation, and — begun in its keyed pass — one
// incremental [`bib::Verifier`] per deferred payload BIB. This decorator
// marries those to the arrival's segment stream: the resident head is
// yielded as the first segment, then each streamed segment feeds the
// `PayloadTail` (payload CRC, block/outer break, anti-smuggling) and the
// block-type-specific data prefix to every verifier, before flowing onward —
// one receiver carries the whole bundle, ready to drive
// `Store::save_stream`. The validation is invisible downstream, surfacing
// only as a pull that fails when the bytes are bad, with the categorised
// verdict settled by `finish` once the drain returns.
pub struct ValidatingReceiver<'a> {
    inner: &'a mut dyn Receiver<Segment>,
    // The resident prefix, yielded as the first segment so one receiver
    // carries the whole bundle. Already validated by the header pass —
    // never absorbed. It may include payload bytes, or (`tail` None) the
    // entire bundle.
    head: Option<Bytes>,
    // The parser's continuation for the unconsumed remainder. `None` means
    // the bundle arrived complete in `head` (the parser took the Ready
    // route and validated everything inline): the head is the only segment
    // and the inner stream is never pulled.
    tail: Option<PayloadTail>,
    // Each deferred payload BIB, paired with its block number for failure
    // attribution.
    verifiers: Vec<(u64, bib::Verifier)>,
    progress: Progress,
}

impl<'a> ValidatingReceiver<'a> {
    // Wraps `inner`, marrying the `tail` continuation and the deferred-BIB
    // `verifiers` to the stream. `head` is the header pass's resident
    // buffer, yielded onward as the first segment. Its payload
    // block-type-specific data prefix (`head[payload_start..]`) — what
    // arrived with the headers, and any peek the pass held — is absorbed
    // into the verifiers here before the stream supplies the rest; the
    // `PayloadTail` was fed it in the header pass, partly by the parser and
    // partly by the hold. A `tail` that already failed on those resident
    // bytes reports its failure when the first segment after the head is
    // absorbed, whatever that segment holds.
    pub fn new(
        inner: &'a mut dyn Receiver<Segment>,
        tail: Option<PayloadTail>,
        mut verifiers: Vec<(u64, bib::Verifier)>,
        head: Bytes,
        payload_start: usize,
    ) -> Self {
        for (_, verifier) in &mut verifiers {
            verifier.update(&head[payload_start..]);
        }
        Self {
            inner,
            head: Some(head),
            tail,
            verifiers,
            progress: Progress::Open,
        }
    }

    // Settle the drain: assert the bundle completed and every deferred BIB
    // verifies. `Ok` once the stream's `Final` has passed with the outer
    // break consumed and each verifier's tag matching; otherwise the
    // categorised [`ValidationFailure`] — an inline structural rejection seen
    // during draining, a payload the stream's `Final` cut short
    // ([`Invalid`](ValidationFailure::Invalid)), a stream that ended before
    // its `Final` ([`Truncated`](ValidationFailure::Truncated)), or a
    // payload-BIB integrity failure.
    pub fn finish(self) -> Result<(), ValidationFailure> {
        match (self.progress, self.tail) {
            (Progress::Failed(failure), _) => return Err(failure),
            // A complete-at-head bundle has no continuation to settle.
            (_, None) => {}
            // An unfinished tail after the stream's `Final` is a short bundle
            // in a complete transfer: invalid, as the header pass treats a
            // payload cut short at `Final`, so the transfer is accepted and
            // the bundle dropped.
            (Progress::Final, Some(tail)) => tail.finish().map_err(ValidationFailure::Invalid)?,
            // Before `Final`, the transfer is unconfirmed, whatever the tail's
            // phase: a truncation, and a resend may complete it.
            (Progress::Open, Some(_)) => return Err(ValidationFailure::Truncated),
        }
        for (bib, verifier) in self.verifiers {
            verifier
                .finish()
                .map_err(|_| ValidationFailure::IntegrityFailed { bib })?;
        }
        Ok(())
    }

    // Validate one segment's bytes: feed the tail (CRC / breaks / trailing
    // data) and the leading body run to every verifier. The body is always
    // consumed from the front of the run, so the `body_remaining` delta is
    // the run's body-prefix length.
    fn absorb(&mut self, bytes: &[u8]) -> Result<(), ValidationFailure> {
        // Only called on inner pulls, which only happen with a continuation.
        let Some(tail) = &mut self.tail else {
            return Err(ValidationFailure::Truncated);
        };
        let before = tail.body_remaining();
        tail.push(bytes).map_err(ValidationFailure::Invalid)?;
        let body_len = (before - tail.body_remaining()) as usize;
        if body_len > 0 {
            for (_, verifier) in &mut self.verifiers {
                verifier.update(&bytes[..body_len]);
            }
        }
        Ok(())
    }
}

#[async_trait]
impl Receiver<Segment> for ValidatingReceiver<'_> {
    async fn recv(&mut self) -> Result<Segment, RecvError> {
        // A failure is terminal: never yield more bytes downstream. After
        // `Final` the stream has nothing left, and a stray pull must not read
        // the producer's departure as a truncation.
        if !matches!(self.progress, Progress::Open) {
            return Err(RecvError);
        }
        // The resident head goes first — validated by the header pass, so it
        // is yielded without absorption. With a continuation more follows;
        // without one the head is the whole bundle, the stream's `Final`, and
        // the inner stream is never pulled.
        if let Some(head) = self.head.take() {
            if self.tail.is_some() {
                return Ok(Segment::Next(head));
            }
            self.progress = Progress::Final;
            return Ok(Segment::Final(head));
        }
        let segment = match self.inner.recv().await {
            Ok(segment) => segment,
            // A producer that goes away before `Final` has abandoned the
            // transfer, even past the outer break.
            Err(e) => {
                self.progress = Progress::Failed(ValidationFailure::Truncated);
                return Err(e);
            }
        };
        let (bytes, last): (&Bytes, bool) = match &segment {
            Segment::Next(bytes) => (bytes, false),
            Segment::Final(bytes) => (bytes, true),
        };
        if let Err(failure) = self.absorb(bytes) {
            self.progress = Progress::Failed(failure);
            return Err(RecvError);
        }
        if last {
            self.progress = Progress::Final;
        }
        Ok(segment)
    }
}

#[cfg(all(test, feature = "rfc9173"))]
mod tests {
    use hardy_bpv7::{
        bpsec::{
            self,
            key::{Key, KeyAlgorithm, KeySet, Operation, Type},
            signer::{Context, Signer},
        },
        builder::Builder,
        crc,
        creation_timestamp::CreationTimestamp,
        // `parse` names the BPA's keyed pass (`crate::bundle::parse`) in
        // this file; alias the bpv7 structural parser it collides with.
        parse::{self as bpv7_parse, BundleParser, ParserProgress},
    };
    use rand::{TryRng, rngs::SysRng};

    use super::*;

    const PAYLOAD: usize = 50_000;
    const CHUNK: usize = 1000;

    // The signing key: its value is immaterial, so it is generated.
    fn sign_key() -> Key {
        let mut k = vec![0u8; 32];
        SysRng.try_fill_bytes(&mut k).unwrap();
        Key {
            key_type: Type::octet_sequence(k),
            key_algorithm: Some(KeyAlgorithm::HS256),
            enc_algorithm: None,
            operations: Some([Operation::Sign, Operation::Verify].into_iter().collect()),
            id: Some("ipn:2.1".into()),
            key_use: None,
        }
    }

    // An oversized-payload bundle, BIB-signed over block 1 with `key` when
    // given.
    fn oversized_bundle(key: Option<&Key>) -> Bytes {
        let (_, base) = Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_payload(vec![0xAB_u8; PAYLOAD].as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();
        let base = Bytes::from(base);
        let Some(key) = key else {
            return base;
        };
        let parsed = bpv7_parse::parse(base).expect("parse the built bundle");
        Bytes::from(
            Signer::new(&parsed.bundle, &parsed.data)
                .sign_block(
                    1,
                    Context::HMAC_SHA2(Default::default()),
                    "ipn:2.1".parse().unwrap(),
                    key,
                )
                .map_err(|(_, e)| e)
                .expect("sign block 1")
                .rebuild()
                .expect("rebuild signed bundle"),
        )
    }

    // Drive the structural parser to `Partial` in CLA-sized chunks, handing
    // back the resident prefix and the tail continuation.
    fn to_partial(full: &Bytes) -> (Bytes, PayloadTail) {
        let mut parser = BundleParser::new(256);
        for chunk in full.chunks(CHUNK) {
            match parser.push(Bytes::copy_from_slice(chunk)).unwrap() {
                ParserProgress::NeedMore(_) => {}
                ParserProgress::Partial { consumed, tail } => return (consumed, tail),
                ParserProgress::Ready(_) => {
                    panic!("a bundle pushed in CLA-sized chunks must reach Partial")
                }
            }
        }
        panic!("parser never reached Partial");
    }

    // Feed `bytes` into a bounded channel as CLA-sized segments (last one
    // `Final`), returning the receiver to hand to an `ValidatingReceiver`.
    async fn segment_stream(bytes: &[u8]) -> hardy_async::channel::Receiver<Segment> {
        let chunks: Vec<&[u8]> = bytes.chunks(CHUNK).collect();
        let (tx, rx) = hardy_async::channel::bounded(chunks.len().max(1));
        let last = chunks.len().saturating_sub(1);
        for (i, c) in chunks.iter().enumerate() {
            let seg = if i == last {
                Segment::Final(Bytes::copy_from_slice(c))
            } else {
                Segment::Next(Bytes::copy_from_slice(c))
            };
            tx.send(seg).await.expect("channel open");
        }
        rx
    }

    // Drain a receiver to completion, returning the concatenated bytes it
    // yielded — the "downstream spool" an `ValidatingReceiver` feeds.
    async fn drain(rx: &mut impl Receiver<Segment>) -> Result<Bytes, RecvError> {
        let mut out = crate::BytesMut::new();
        loop {
            match rx.recv().await? {
                Segment::Next(b) => out.extend_from_slice(&b),
                Segment::Final(b) => {
                    out.extend_from_slice(&b);
                    return Ok(out.freeze());
                }
            }
        }
    }

    // A valid tail passes through byte-for-byte — the resident head first,
    // then the streamed remainder — and settles Ok.
    #[tokio::test]
    async fn valid_tail_passes_through_and_settles() {
        let full = oversized_bundle(None);
        let (consumed, tail) = to_partial(&full);
        let rest = full.slice(consumed.len()..);

        let mut inner = segment_stream(&rest).await;
        let payload_start = consumed.len();
        let mut tr =
            ValidatingReceiver::new(&mut inner, Some(tail), Vec::new(), consumed, payload_start);
        let yielded = drain(&mut tr).await.expect("valid tail drains");
        assert_eq!(
            yielded, full,
            "the head then every streamed byte is yielded onward unchanged"
        );
        tr.finish().expect("a well-formed tail settles Ok");
    }

    // A complete-at-head bundle yields the head as its only, Final segment
    // and settles Ok without pulling the inner stream.
    #[tokio::test]
    async fn complete_at_head_yields_one_final_segment() {
        let full = oversized_bundle(None);

        // An inner stream that errors if ever pulled.
        let (tx, mut rx) = hardy_async::channel::bounded::<Segment>(1);
        drop(tx);

        let payload_start = full.len();
        let mut tr =
            ValidatingReceiver::new(&mut rx, None, Vec::new(), full.clone(), payload_start);
        let yielded = drain(&mut tr).await.expect("the head drains as Final");
        assert_eq!(yielded, full, "the head is the whole bundle");
        assert!(tr.recv().await.is_err(), "nothing follows the head");
        tr.finish().expect("a complete bundle settles Ok");
    }

    // A flipped payload byte fails the payload CRC — a complete-but-invalid
    // bundle (accepted then dropped), reported as `Invalid`.
    #[tokio::test]
    async fn corrupt_payload_is_invalid() {
        let full = oversized_bundle(None);
        let (consumed, tail) = to_partial(&full);
        let mut rest = full.slice(consumed.len()..).to_vec();
        rest[10] ^= 0xFF; // inside the streamed body, before the CRC/breaks

        let mut inner = segment_stream(&rest).await;
        let payload_start = consumed.len();
        let mut tr =
            ValidatingReceiver::new(&mut inner, Some(tail), Vec::new(), consumed, payload_start);
        // The corruption surfaces at the CRC check (end of body) as a failed
        // pull; finish categorises it.
        let _ = drain(&mut tr).await;
        let failure = tr.finish().expect_err("a payload CRC mismatch is Invalid");
        assert!(matches!(
            failure,
            ValidationFailure::Invalid(hardy_bpv7::Error::InvalidCrc(crc::Error::IncorrectCrc))
        ));
        assert_eq!(
            failure.reason_code(),
            Some(ReasonCode::BlockUnintelligible),
            "a CRC mismatch reports the generic block reason"
        );
    }

    // A producer that drops before the outer break is a truncation.
    #[tokio::test]
    async fn short_stream_is_truncated() {
        let full = oversized_bundle(None);
        let (consumed, tail) = to_partial(&full);
        let rest = full.slice(consumed.len()..);

        // Send only the first chunk, then drop the sender (no `Final`).
        let (tx, mut rx) = hardy_async::channel::bounded(1);
        tx.send(Segment::Next(rest.slice(..CHUNK)))
            .await
            .expect("channel open");
        drop(tx);

        let payload_start = consumed.len();
        let mut tr =
            ValidatingReceiver::new(&mut rx, Some(tail), Vec::new(), consumed, payload_start);
        assert!(
            matches!(tr.recv().await, Ok(Segment::Next(_))),
            "first pull yields the resident head"
        );
        assert!(
            matches!(tr.recv().await, Ok(Segment::Next(_))),
            "second pull yields the streamed chunk"
        );
        assert!(
            tr.recv().await.is_err(),
            "the dropped producer ends the stream"
        );
        let failure = tr.finish().expect_err("an unfinished tail is Truncated");
        assert!(matches!(failure, ValidationFailure::Truncated));
        assert_eq!(
            failure.reason_code(),
            None,
            "a refused transfer raises no status report"
        );
    }

    // A producer that drops past the outer break but before `Final` has
    // abandoned the transfer: still a truncation, so the transfer is refused.
    #[tokio::test]
    async fn stream_ending_without_final_is_truncated() {
        let full = oversized_bundle(None);
        let (consumed, tail) = to_partial(&full);
        let rest = full.slice(consumed.len()..);

        // The whole remainder arrives in `Next` segments; the producer then
        // drops without sending `Final`.
        let chunks: Vec<&[u8]> = rest.chunks(CHUNK).collect();
        let (tx, mut rx) = hardy_async::channel::bounded(chunks.len());
        for chunk in chunks {
            tx.send(Segment::Next(Bytes::copy_from_slice(chunk)))
                .await
                .expect("channel open");
        }
        drop(tx);

        let payload_start = consumed.len();
        let mut tr =
            ValidatingReceiver::new(&mut rx, Some(tail), Vec::new(), consumed, payload_start);
        assert!(
            matches!(drain(&mut tr).await, Err(RecvError)),
            "the stream ends without `Final`"
        );
        let failure = tr
            .finish()
            .expect_err("a stream without `Final` is Truncated");
        assert!(matches!(failure, ValidationFailure::Truncated));
    }

    // A transfer that delivers its `Final` with the payload short is
    // complete: the bundle is invalid (accepted and dropped, reported as an
    // unintelligible block), not truncated, so the peer does not resend it.
    #[tokio::test]
    async fn a_short_final_is_invalid_not_truncated() {
        let full = oversized_bundle(None);
        let (consumed, tail) = to_partial(&full);
        // The remainder less its last 100 bytes, ending in a `Final`: short
        // by more than the 6-byte trailer, so the cut lands in the body.
        let short = full.slice(consumed.len()..full.len() - 100);

        let mut inner = segment_stream(&short).await;
        let payload_start = consumed.len();
        let mut tr =
            ValidatingReceiver::new(&mut inner, Some(tail), Vec::new(), consumed, payload_start);
        drain(&mut tr)
            .await
            .expect("the short stream drains to its `Final`");
        let failure = tr.finish().expect_err("a short bundle does not settle Ok");
        assert!(
            matches!(
                failure,
                ValidationFailure::Invalid(hardy_bpv7::Error::InvalidCBOR(_))
            ),
            "a payload cut short at `Final` is Invalid, got {failure:?}"
        );
        assert_eq!(failure.reason_code(), Some(ReasonCode::BlockUnintelligible));
    }

    // A stray pull after `Final` finds the producer gone (the stream's
    // sender dropped once it was sent). The pull fails without recording a
    // truncation: the transfer completed, and settles Ok.
    #[tokio::test]
    async fn a_pull_after_final_does_not_latch() {
        let full = oversized_bundle(None);
        let (consumed, tail) = to_partial(&full);
        let rest = full.slice(consumed.len()..);

        let mut inner = segment_stream(&rest).await;
        let payload_start = consumed.len();
        let mut tr =
            ValidatingReceiver::new(&mut inner, Some(tail), Vec::new(), consumed, payload_start);
        drain(&mut tr)
            .await
            .expect("the tail drains to its `Final`");
        assert!(
            matches!(tr.recv().await, Err(RecvError)),
            "nothing follows `Final`"
        );
        tr.finish()
            .expect("a complete transfer settles Ok after a stray pull");
    }

    // A drain settled before the stream's `Final`, every pull having
    // succeeded, is an unfinished transfer: Truncated, never a short bundle.
    #[tokio::test]
    async fn finish_before_final_is_truncated() {
        let full = oversized_bundle(None);
        let (consumed, tail) = to_partial(&full);
        let rest = full.slice(consumed.len()..);

        let mut inner = segment_stream(&rest).await;
        let payload_start = consumed.len();
        let mut tr =
            ValidatingReceiver::new(&mut inner, Some(tail), Vec::new(), consumed, payload_start);
        assert!(
            matches!(tr.recv().await, Ok(Segment::Next(_))),
            "the head yields first"
        );
        let failure = tr
            .finish()
            .expect_err("an unfinished tail before `Final` does not settle Ok");
        assert!(
            matches!(failure, ValidationFailure::Truncated),
            "got {failure:?}"
        );
    }

    // A drain settled with every byte through the outer break pulled in
    // `Next` segments, its `Final` never pulled, is an unconfirmed transfer:
    // Truncated, though the tail is complete.
    #[tokio::test]
    async fn finish_at_a_complete_tail_before_final_is_truncated() {
        let full = oversized_bundle(None);
        let (consumed, tail) = to_partial(&full);
        let rest = full.slice(consumed.len()..);

        let chunks: Vec<&[u8]> = rest.chunks(CHUNK).collect();
        let (tx, mut rx) = hardy_async::channel::bounded(chunks.len() + 1);
        for chunk in &chunks {
            tx.send(Segment::Next(Bytes::copy_from_slice(chunk)))
                .await
                .expect("channel open");
        }
        tx.send(Segment::Final(Bytes::new()))
            .await
            .expect("channel open");

        let payload_start = consumed.len();
        let mut tr =
            ValidatingReceiver::new(&mut rx, Some(tail), Vec::new(), consumed, payload_start);
        // The resident head, then every streamed chunk.
        for _ in 0..=chunks.len() {
            assert!(matches!(tr.recv().await, Ok(Segment::Next(_))));
        }
        let failure = tr
            .finish()
            .expect_err("a transfer whose `Final` was never pulled does not settle Ok");
        assert!(
            matches!(failure, ValidationFailure::Truncated),
            "got {failure:?}"
        );
    }

    // The trailer can straddle segments. The Builder's payload block is a
    // definite-length array with a CRC-32, so the bundle ends in six bytes:
    // the CRC head `0x44`, the 4-byte CRC value, and the outer break `0xFF`.
    // Split before each of them and the tail still settles Ok, byte for byte.
    #[tokio::test]
    async fn a_trailer_straddling_segments_settles() {
        const TRAILER: usize = 6;
        let full = oversized_bundle(None);
        let (consumed, _) = to_partial(&full);
        let rest = full.slice(consumed.len()..);
        assert_eq!(rest[rest.len() - TRAILER], 0x44, "the CRC-32 head");
        assert_eq!(rest[rest.len() - 1], 0xFF, "the outer break");
        for from_end in 1..=TRAILER {
            let split = rest.len() - from_end;
            let (consumed, tail) = to_partial(&full);
            let (tx, mut rx) = hardy_async::channel::bounded(2);
            tx.send(Segment::Next(rest.slice(..split)))
                .await
                .expect("channel open");
            tx.send(Segment::Final(rest.slice(split..)))
                .await
                .expect("channel open");

            let payload_start = consumed.len();
            let mut tr =
                ValidatingReceiver::new(&mut rx, Some(tail), Vec::new(), consumed, payload_start);
            let yielded = drain(&mut tr).await.expect("the tail drains");
            assert_eq!(yielded, full, "split {split}: every byte is yielded");
            tr.finish()
                .unwrap_or_else(|e| panic!("split {split}: the tail settles Ok, got {e:?}"));
        }
    }

    // The reason a drain failure hands the reporting path: a failed deferred
    // BIB is a failed security operation (RFC 9172).
    #[test]
    fn integrity_failure_reports_failed_security_operation() {
        assert_eq!(
            ValidationFailure::IntegrityFailed { bib: 3 }.reason_code(),
            Some(ReasonCode::FailedSecurityOperation)
        );
    }

    // Bytes past the outer break are trailing data — rejected as Invalid.
    #[tokio::test]
    async fn trailing_data_is_invalid() {
        let full = oversized_bundle(None);
        let (consumed, tail) = to_partial(&full);
        let mut rest = full.slice(consumed.len()..).to_vec();
        rest.push(0x00); // one byte past the bundle's outer break

        let mut inner = segment_stream(&rest).await;
        let payload_start = consumed.len();
        let mut tr =
            ValidatingReceiver::new(&mut inner, Some(tail), Vec::new(), consumed, payload_start);
        let _ = drain(&mut tr).await;
        assert!(
            matches!(
                tr.finish(),
                Err(ValidationFailure::Invalid(
                    hardy_bpv7::Error::AdditionalData
                ))
            ),
            "bytes after the outer break are Invalid"
        );
    }

    // Build the deferred-BIB verifiers for a bundle signed with `key`: the
    // keyed header pass begins them itself. Returns the verifiers, the header
    // prefix, the tail, and the resident body-prefix offset.
    async fn signed_setup(
        full: &Bytes,
        key: &Key,
        peek: usize,
    ) -> (Vec<(u64, bib::Verifier)>, Bytes, PayloadTail, usize) {
        let key = key.clone();
        let keys = move |_: &hardy_bpv7::Bundle, _: &[u8]| -> Box<dyn bpsec::key::KeySource> {
            Box::new(KeySet::new(vec![key]))
        };
        let mut rx = segment_stream(full).await;
        let (hv, headers, tail, _) = parse::parse_headers(&mut rx, 1 << 20, peek, keys)
            .await
            .map_err(|_| ())
            .expect("header pass verifies (payload deferred)");
        let tail = tail.expect("a payload segmented past its header takes the Partial route");
        assert!(
            !hv.deferred_verifiers.is_empty(),
            "the payload BIB is deferred"
        );

        // The payload body prefix already resident in `headers`.
        let payload_start = hv.bundle.blocks.get(&1).unwrap().payload_range().start as usize;
        (hv.deferred_verifiers, headers, tail, payload_start)
    }

    // A signed bundle whose first segment ends where the payload's body does:
    // the body is resident at the header pass, so the payload BIB verifies
    // there and none is deferred, and no verifier reads the resident bytes
    // past the payload's start, trailer included. The signer strips its
    // target's CRC, so the trailer is the outer break alone. The drain then
    // settles Ok.
    #[tokio::test]
    async fn a_body_resident_at_the_header_pass_defers_no_bib() {
        let key = sign_key();
        let full = oversized_bundle(Some(&key));
        let payload = bpv7_parse::parse(full.clone()).unwrap().bundle.blocks[&1].payload_range();
        let body_end = payload.end as usize;
        assert_eq!(body_end, full.len() - 1, "the trailer is the outer break");

        let (tx, mut rx) = hardy_async::channel::bounded(2);
        tx.send(Segment::Next(full.slice(..body_end)))
            .await
            .expect("channel open");
        tx.send(Segment::Final(full.slice(body_end..)))
            .await
            .expect("channel open");
        let keys = move |_: &hardy_bpv7::Bundle, _: &[u8]| -> Box<dyn bpsec::key::KeySource> {
            Box::new(KeySet::new(vec![key]))
        };
        let (hv, headers, tail, _) = parse::parse_headers(&mut rx, 1 << 20, 0, keys)
            .await
            .map_err(|_| ())
            .expect("the header pass verifies the resident payload");
        let tail = tail.expect("the outer break is still to come");
        assert!(
            hv.deferred_verifiers.is_empty(),
            "a payload resident at the header pass is verified there"
        );

        let mut tr = ValidatingReceiver::new(
            &mut rx,
            Some(tail),
            hv.deferred_verifiers,
            headers,
            payload.start as usize,
        );
        drain(&mut tr).await.expect("the outer break drains");
        tr.finish().expect("the bundle settles Ok");
    }

    // A deferred payload BIB verifies over the streamed body: the resident
    // prefix plus the streamed remainder feed the digest, and `finish`
    // settles Ok.
    #[tokio::test]
    async fn deferred_bib_verifies_over_stream() {
        let key = sign_key();
        let full = oversized_bundle(Some(&key));
        let (verifiers, headers, tail, payload_start) = signed_setup(&full, &key, 0).await;
        assert_eq!(verifiers.len(), 1);

        let rest = full.slice(headers.len()..);
        let mut inner = segment_stream(&rest).await;
        let mut tr =
            ValidatingReceiver::new(&mut inner, Some(tail), verifiers, headers, payload_start);
        drain(&mut tr).await.expect("valid signed tail drains");
        tr.finish().expect("the deferred payload BIB verifies");
    }

    // Tampering a streamed body byte fails the deferred BIB at settle, named
    // by the BIB that made the claim. The Signer strips its target's CRC, so
    // the integrity check is the only one the tamper can fail.
    #[tokio::test]
    async fn deferred_bib_tamper_fails() {
        let key = sign_key();
        let full = oversized_bundle(Some(&key));
        let (verifiers, headers, tail, payload_start) = signed_setup(&full, &key, 0).await;
        let signed_by = verifiers[0].0;

        let mut rest = full.slice(headers.len()..).to_vec();
        rest[5] ^= 0xFF; // inside the streamed body, after the header prefix
        let mut inner = segment_stream(&rest).await;
        let mut tr =
            ValidatingReceiver::new(&mut inner, Some(tail), verifiers, headers, payload_start);
        drain(&mut tr).await.expect("a CRC-free payload drains");
        assert!(
            matches!(tr.finish(), Err(ValidationFailure::IntegrityFailed { bib }) if bib == signed_by),
            "the tampered payload fails the BIB that signed it"
        );
    }

    // A held peek is body the deferred BIB has not yet covered: the header
    // pass feeds it to the verifiers once, with the rest of the body to
    // follow through the drain, and `finish` settles Ok.
    #[tokio::test]
    async fn deferred_bib_verifies_over_a_held_peek() {
        let key = sign_key();
        let full = oversized_bundle(Some(&key));
        let (verifiers, headers, tail, payload_start) = signed_setup(&full, &key, 2500).await;
        let held = headers.len() - payload_start;
        assert!(held >= 2500, "the peek holds 2500 body bytes, got {held}");

        let rest = full.slice(headers.len()..);
        let mut inner = segment_stream(&rest).await;
        let mut tr =
            ValidatingReceiver::new(&mut inner, Some(tail), verifiers, headers, payload_start);
        drain(&mut tr).await.expect("valid signed tail drains");
        tr.finish().expect("the deferred payload BIB verifies");
    }

    // A tampered byte inside the held peek fails the deferred BIB: the peek's
    // bytes are part of what it verifies.
    #[tokio::test]
    async fn a_tamper_inside_the_peek_fails_the_deferred_bib() {
        let key = sign_key();
        let mut tampered = oversized_bundle(Some(&key)).to_vec();
        let payload_start = bpv7_parse::parse(Bytes::from(tampered.clone()))
            .unwrap()
            .bundle
            .blocks[&1]
            .payload_range()
            .start as usize;
        tampered[payload_start + 1500] ^= 0xFF;
        let tampered = Bytes::from(tampered);
        let (verifiers, headers, tail, payload_start) = signed_setup(&tampered, &key, 2500).await;
        assert!(
            headers.len() - payload_start > 1500,
            "the tamper lies inside the peek"
        );
        let signed_by = verifiers[0].0;

        let rest = tampered.slice(headers.len()..);
        let mut inner = segment_stream(&rest).await;
        let mut tr =
            ValidatingReceiver::new(&mut inner, Some(tail), verifiers, headers, payload_start);
        drain(&mut tr).await.expect("a CRC-free payload drains");
        assert!(
            matches!(tr.finish(), Err(ValidationFailure::IntegrityFailed { bib }) if bib == signed_by),
            "the tampered peek fails the BIB that signed it"
        );
    }

    #[test]
    fn validating_receiver_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<ValidatingReceiver<'static>>();
    }
}
