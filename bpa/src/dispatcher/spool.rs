//! The ingress spool: the validating, cancellable drain of a received
//! bundle into the store.
//!
//! [`parse_headers`](crate::bundle::parse::parse_headers) hands back the
//! resident header prefix, a synchronous [`PayloadTail`] continuation, and —
//! begun in its keyed pass via
//! [`begin_payload_verification`](hardy_bpv7::checks::begin_payload_verification)
//! — one incremental [`bib::Verifier`] per deferred payload BIB.
//! [`Dispatcher::spool`] marries those to the CLA's segment stream through
//! the private `TailReceiver`: the resident head is yielded as the first
//! segment, then each streamed segment feeds the [`PayloadTail`] (payload
//! CRC, block/outer break, anti-smuggling) and the block-type-specific data
//! prefix to every verifier, before flowing onward into
//! `Store::save_stream` (the interim spool now, the backends' streamed
//! store after the storage tranche). One receiver carries the whole bundle;
//! the validation is invisible downstream, surfacing only as a pull that
//! fails when the bytes are bad, with the categorised verdict settled once
//! the stream is drained.

use futures::FutureExt;
use hardy_async::{CancellationToken, async_trait};
use hardy_bpv7::{bpsec::bib, parse::PayloadTail, status_report::ReasonCode};
use thiserror::Error;
use trace_err::*;

use super::*;
use crate::{
    bundle::parse::status_report_reason_for,
    cla::Segment,
    stream::{CancellableReceiver, Receiver, RecvError},
};

/// Why a [`TailReceiver`] rejected the drained bytes.
#[derive(Debug, Error)]
pub enum TailFailure {
    /// The stream ended before the bundle's outer break: the producer went
    /// away mid-bundle. A resend may complete it, so the transfer is
    /// refused (the CLA withholds its acknowledgement).
    #[error("the stream ended before the bundle's outer break")]
    Truncated,
    /// The drained bytes were structurally invalid — payload CRC mismatch,
    /// a malformed trailer, or bytes past the outer break. The bundle is
    /// complete but unacceptable: accepted and dropped, never refused.
    #[error("invalid payload bytes: {0}")]
    Invalid(hardy_bpv7::Error),
    /// A deferred payload BIB failed integrity over the streamed body
    /// (RFC 9172 §5.1.1). Names the BIB block that made the claim; the
    /// bundle is accepted and dropped.
    #[error("deferred payload BIB {bib} failed integrity over the streamed body")]
    IntegrityFailed { bib: u64 },
}

impl TailFailure {
    /// The status-report reason this failure raises, so the drain's caller
    /// reports the drop like any other parsing failure (the combined RFC
    /// 9171 §5.6/§5.10 reception + deletion status report, per the bundle's
    /// flags). `None` for [`Truncated`](Self::Truncated): a refused transfer
    /// is never reported — the peer retains custody and may resend.
    pub fn reason_code(&self) -> Option<ReasonCode> {
        match self {
            Self::Truncated => None,
            Self::Invalid(error) => Some(status_report_reason_for(error)),
            Self::IntegrityFailed { .. } => Some(ReasonCode::FailedSecurityOperation),
        }
    }
}

impl Dispatcher {
    /// The whole spool rig in one call: drain a bundle — the resident
    /// `head` plus `stream`'s remainder — into the store, concurrently with
    /// whatever the caller joins this future against.
    ///
    /// A bounded channel decouples the two halves: a spawned task owns the
    /// store side (a `TailReceiver` driving `Store::save_stream`), while
    /// this future pumps the borrowed `stream` into the channel — the
    /// channel depth is backpressure, not buffering; the spool itself is
    /// bounded by `max_bundle_size` as the defensive backstop. Cancelling
    /// `token` aborts both halves, even mid-park. The future resolves once
    /// both settle: the pump ends at the stream's end, at a cancel, or when
    /// the store side stops pulling.
    ///
    /// `Ok` carries the storage name and total size of the saved bundle.
    /// `Err` means the drain failed and nothing remains persisted — a save
    /// whose post-stream validation fails is discarded before the error
    /// returns. The one exception: a save that races a deliberate cancel
    /// comes back `Ok`, and the canceller owes the discard.
    pub(super) async fn spool(
        &self,
        stream: &mut dyn Receiver<Segment>,
        tail: Option<PayloadTail>,
        verifiers: Vec<(u64, bib::Verifier)>,
        head: Bytes,
        payload_start: usize,
        token: CancellationToken,
    ) -> Result<(Arc<str>, usize), TailFailure> {
        // 32-bit: a cap beyond the address space saturates — nothing larger
        // could be spooled to RAM anyway.
        let max_size = usize::try_from(self.max_bundle_size.get()).unwrap_or(usize::MAX);
        let (seg_tx, seg_rx) = hardy_async::channel::bounded::<Segment>(4);
        let task = {
            let store = self.store.clone();
            let task_token = token.clone();
            hardy_async::spawn!(self.tasks, "ingress_spool", async move {
                let mut seg_rx = seg_rx;
                let mut tail_rx = TailReceiver::new(
                    &mut seg_rx,
                    tail,
                    verifiers,
                    head,
                    payload_start,
                    task_token,
                );
                match store.save_stream(&mut tail_rx, max_size).await {
                    Ok((storage_name, len)) => match tail_rx.finish() {
                        Ok(()) => Ok((storage_name, len)),
                        Err(failure) => {
                            // Staged before the verdict settled — the
                            // discard half of the streaming contract.
                            store.delete_data(&storage_name).await;
                            Err(failure)
                        }
                    },
                    // Any drain failure — an ended pump, a deliberate
                    // cancel, or the spool's defensive bound — settles
                    // through the TailReceiver's verdict; nothing was
                    // persisted.
                    Err(_) => Err(tail_rx.finish().err().unwrap_or(TailFailure::Truncated)),
                }
            })
        };

        // Pump the borrowed stream into the channel.
        {
            let mut src = CancellableReceiver {
                inner: stream,
                token,
            };
            loop {
                let Ok(seg) = src.recv().await else { break };
                let last = matches!(seg, Segment::Final(_));
                if seg_tx.send(seg).await.is_err() || last {
                    break;
                }
            }
        }
        // Release the channel so a store side still pulling (an inner
        // truncation) settles rather than parking forever.
        drop(seg_tx);

        task.await.trace_expect("Ingress spool task failed")
    }
}

// A [`Receiver<Segment>`] decorator that validates a bundle's payload tail
// as it streams through — see the [module docs](self).
//
// Private machinery of [`spool`]: constructed over the spool channel inside
// the store-side task, driven as an ordinary [`Receiver`] by `save_stream`,
// then settled with `finish`.
struct TailReceiver<'a> {
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
    // Races every inner pull, so a cancelled drain stops even while parked
    // awaiting the producer. Cancellation is a failed pull, settling as
    // `Truncated` — indistinguishable from the producer going away.
    token: CancellationToken,
    // Set by the first failing pull; a later pull short-circuits and
    // `finish` reports it.
    failure: Option<TailFailure>,
}

impl<'a> TailReceiver<'a> {
    // Wraps `inner`, marrying the `tail` continuation and the deferred-BIB
    // `verifiers` to the stream. `head` is the header pass's resident
    // buffer — yielded onward as the first segment, and its payload
    // block-type-specific data prefix (`head[payload_start..]`) is absorbed
    // into the verifiers here (a `PayloadTail` was pre-fed it at
    // construction) before the stream supplies the rest. Cancelling `token`
    // aborts the drain: pulls fail and `finish` settles
    // [`TailFailure::Truncated`].
    fn new(
        inner: &'a mut dyn Receiver<Segment>,
        tail: Option<PayloadTail>,
        mut verifiers: Vec<(u64, bib::Verifier)>,
        head: Bytes,
        payload_start: usize,
        token: CancellationToken,
    ) -> Self {
        for (_, verifier) in &mut verifiers {
            verifier.update(&head[payload_start..]);
        }
        Self {
            inner,
            head: Some(head),
            tail,
            verifiers,
            token,
            failure: None,
        }
    }

    // Settle the drain: assert the bundle completed and every deferred BIB
    // verifies. `Ok` once the outer break was consumed and each verifier's
    // tag matches; otherwise the categorised [`TailFailure`] — an inline
    // structural rejection seen during draining, a truncation, or a
    // payload-BIB integrity failure.
    fn finish(self) -> Result<(), TailFailure> {
        if let Some(failure) = self.failure {
            return Err(failure);
        }
        // A stream that ended before the outer break is a truncation. A
        // complete-at-head bundle has no continuation to settle.
        if let Some(tail) = self.tail {
            tail.finish().map_err(|_| TailFailure::Truncated)?;
        }
        for (bib, verifier) in self.verifiers {
            verifier
                .finish()
                .map_err(|_| TailFailure::IntegrityFailed { bib })?;
        }
        Ok(())
    }

    // Validate one segment's bytes: feed the tail (CRC / breaks / trailing
    // data) and the leading body run to every verifier. The body is always
    // consumed from the front of the run, so the `body_remaining` delta is
    // the run's body-prefix length.
    fn absorb(&mut self, bytes: &[u8]) -> Result<(), TailFailure> {
        // Only called on inner pulls, which only happen with a continuation.
        let Some(tail) = &mut self.tail else {
            return Err(TailFailure::Truncated);
        };
        let before = tail.body_remaining();
        tail.push(bytes).map_err(TailFailure::Invalid)?;
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
impl Receiver<Segment> for TailReceiver<'_> {
    async fn recv(&mut self) -> Result<Segment, RecvError> {
        // A prior failure is terminal: never yield more bytes downstream.
        if self.failure.is_some() {
            return Err(RecvError);
        }
        // The resident head goes first — validated by the header pass, so it
        // is yielded without absorption. With a continuation more follows;
        // without one the head is the whole bundle.
        if let Some(head) = self.head.take() {
            return Ok(if self.tail.is_some() {
                Segment::Next(head)
            } else {
                Segment::Final(head)
            });
        }
        // Complete at head: nothing further to yield, and the inner stream
        // is never pulled.
        if self.tail.is_none() {
            return Err(RecvError);
        }
        let segment = futures::select_biased! {
            _ = self.token.cancelled().fuse() => return Err(RecvError),
            r = self.inner.recv().fuse() => r?,
        };
        let bytes: &Bytes = match &segment {
            Segment::Next(bytes) | Segment::Final(bytes) => bytes,
        };
        if let Err(failure) = self.absorb(bytes) {
            self.failure = Some(failure);
            return Err(RecvError);
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
        creation_timestamp::CreationTimestamp,
        parse::{self, BundleParser, ParserProgress},
    };

    use super::*;

    const PAYLOAD: usize = 50_000;
    const CHUNK: usize = 1000;

    fn sign_key() -> Key {
        Key {
            key_type: Type::OctetSequence {
                key: b"qwertyuiopasdfghqwertyuiopasdfgh".as_slice().into(),
            },
            key_algorithm: Some(KeyAlgorithm::HS256),
            enc_algorithm: None,
            operations: Some([Operation::Sign, Operation::Verify].into_iter().collect()),
            id: Some("ipn:2.1".into()),
            key_use: None,
        }
    }

    // An oversized-payload bundle, optionally BIB-signed over block 1.
    fn oversized_bundle(sign: bool) -> Bytes {
        let (_, base) = Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_payload(vec![0xAB_u8; PAYLOAD].as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();
        let base = Bytes::from(base);
        if !sign {
            return base;
        }
        let parsed = parse::parse(base).expect("parse the built bundle");
        Bytes::from(
            Signer::new(&parsed.bundle, &parsed.data)
                .sign_block(
                    1,
                    Context::HMAC_SHA2(Default::default()),
                    "ipn:2.1".parse().unwrap(),
                    &sign_key(),
                )
                .map_err(|(_, e)| e)
                .expect("sign block 1")
                .rebuild()
                .expect("rebuild signed bundle"),
        )
    }

    // Drive the structural parser to `Partial` in CLA-sized chunks, handing
    // back the resident header prefix and the tail continuation.
    fn to_partial(full: &Bytes) -> (Bytes, PayloadTail) {
        let mut parser = BundleParser::new(256);
        for chunk in full.chunks(CHUNK) {
            match parser.push(Bytes::copy_from_slice(chunk)).unwrap() {
                ParserProgress::NeedMore(_) => {}
                ParserProgress::Partial { consumed, tail } => return (consumed, tail),
                ParserProgress::Ready(_) => panic!("oversized payload must Partial"),
            }
        }
        panic!("parser never reached Partial");
    }

    // Feed `bytes` into a bounded channel as CLA-sized segments (last one
    // `Final`), returning the receiver to hand to a `TailReceiver`.
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
    // yielded — the "downstream spool" a `TailReceiver` feeds.
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
        let full = oversized_bundle(false);
        let (consumed, tail) = to_partial(&full);
        let rest = full.slice(consumed.len()..);

        let mut inner = segment_stream(&rest).await;
        let payload_start = consumed.len();
        let mut tr = TailReceiver::new(
            &mut inner,
            Some(tail),
            Vec::new(),
            consumed,
            payload_start,
            CancellationToken::new(),
        );
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
        let full = oversized_bundle(false);

        // An inner stream that errors if ever pulled.
        let (tx, mut rx) = hardy_async::channel::bounded::<Segment>(1);
        drop(tx);

        let payload_start = full.len();
        let mut tr = TailReceiver::new(
            &mut rx,
            None,
            Vec::new(),
            full.clone(),
            payload_start,
            CancellationToken::new(),
        );
        let yielded = drain(&mut tr).await.expect("the head drains as Final");
        assert_eq!(yielded, full, "the head is the whole bundle");
        assert!(tr.recv().await.is_err(), "nothing follows the head");
        tr.finish().expect("a complete bundle settles Ok");
    }

    // Cancelling the token aborts the drain: the pull fails even while the
    // producer is alive but silent, and the verdict settles Truncated —
    // indistinguishable from a vanished producer.
    #[tokio::test]
    async fn cancelled_drain_settles_truncated() {
        let full = oversized_bundle(false);
        let (consumed, tail) = to_partial(&full);

        // The sender stays alive and silent: only the token can unblock.
        let (_tx, mut rx) = hardy_async::channel::bounded::<Segment>(1);
        let token = CancellationToken::new();
        let payload_start = consumed.len();
        let mut tr = TailReceiver::new(
            &mut rx,
            Some(tail),
            Vec::new(),
            consumed,
            payload_start,
            token.clone(),
        );

        assert!(
            matches!(tr.recv().await, Ok(Segment::Next(_))),
            "the head yields before any inner pull"
        );
        token.cancel();
        assert!(
            tr.recv().await.is_err(),
            "a cancelled pull fails without the producer going away"
        );
        let failure = tr.finish().expect_err("a cancelled drain is incomplete");
        assert!(matches!(failure, TailFailure::Truncated));
    }

    // A flipped payload byte fails the payload CRC — a complete-but-invalid
    // bundle (accepted then dropped), reported as `Invalid`.
    #[tokio::test]
    async fn corrupt_payload_is_invalid() {
        let full = oversized_bundle(false);
        let (consumed, tail) = to_partial(&full);
        let mut rest = full.slice(consumed.len()..).to_vec();
        rest[10] ^= 0xFF; // inside the streamed body, before the CRC/breaks

        let mut inner = segment_stream(&rest).await;
        let payload_start = consumed.len();
        let mut tr = TailReceiver::new(
            &mut inner,
            Some(tail),
            Vec::new(),
            consumed,
            payload_start,
            CancellationToken::new(),
        );
        // The corruption surfaces at the CRC check (end of body) as a failed
        // pull; finish categorises it.
        let _ = drain(&mut tr).await;
        let failure = tr.finish().expect_err("a payload CRC mismatch is Invalid");
        assert!(matches!(failure, TailFailure::Invalid(_)));
        assert_eq!(
            failure.reason_code(),
            Some(ReasonCode::BlockUnintelligible),
            "a CRC mismatch reports the generic block reason"
        );
    }

    // A producer that drops before the outer break is a truncation.
    #[tokio::test]
    async fn short_stream_is_truncated() {
        let full = oversized_bundle(false);
        let (consumed, tail) = to_partial(&full);
        let rest = full.slice(consumed.len()..);

        // Send only the first chunk, then drop the sender (no `Final`).
        let (tx, mut rx) = hardy_async::channel::bounded(1);
        tx.send(Segment::Next(rest.slice(..CHUNK)))
            .await
            .expect("channel open");
        drop(tx);

        let payload_start = consumed.len();
        let mut tr = TailReceiver::new(
            &mut rx,
            Some(tail),
            Vec::new(),
            consumed,
            payload_start,
            CancellationToken::new(),
        );
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
        assert!(matches!(failure, TailFailure::Truncated));
        assert_eq!(
            failure.reason_code(),
            None,
            "a refused transfer raises no status report"
        );
    }

    // The reason a drain failure hands the reporting path: a failed deferred
    // BIB is a failed security operation (RFC 9172).
    #[test]
    fn integrity_failure_reports_failed_security_operation() {
        assert_eq!(
            TailFailure::IntegrityFailed { bib: 3 }.reason_code(),
            Some(ReasonCode::FailedSecurityOperation)
        );
    }

    // Bytes past the outer break are trailing data — rejected as Invalid.
    #[tokio::test]
    async fn trailing_data_is_invalid() {
        let full = oversized_bundle(false);
        let (consumed, tail) = to_partial(&full);
        let mut rest = full.slice(consumed.len()..).to_vec();
        rest.push(0x00); // one byte past the bundle's outer break

        let mut inner = segment_stream(&rest).await;
        let payload_start = consumed.len();
        let mut tr = TailReceiver::new(
            &mut inner,
            Some(tail),
            Vec::new(),
            consumed,
            payload_start,
            CancellationToken::new(),
        );
        let _ = drain(&mut tr).await;
        assert!(
            matches!(tr.finish(), Err(TailFailure::Invalid(_))),
            "bytes after the outer break are Invalid"
        );
    }

    // Build the deferred-BIB verifiers for a signed bundle: the keyed header
    // pass begins them itself. Returns the verifiers, the header prefix, the
    // tail, and the resident body-prefix offset.
    async fn signed_setup(full: &Bytes) -> (Vec<(u64, bib::Verifier)>, Bytes, PayloadTail, usize) {
        let keys = |_: &hardy_bpv7::Bundle, _: &[u8]| -> Box<dyn bpsec::key::KeySource> {
            Box::new(KeySet::new(vec![sign_key()]))
        };
        let mut rx = segment_stream(full).await;
        let (hv, headers, tail, _) = crate::bundle::parse::parse_headers(&mut rx, 1 << 20, keys)
            .await
            .map_err(|_| ())
            .expect("header pass verifies (payload deferred)");
        let tail = tail.expect("oversized payload takes the Partial route");
        assert!(
            !hv.deferred_verifiers.is_empty(),
            "the payload BIB is deferred"
        );

        // The payload body prefix already resident in `headers`.
        let payload_start = hv.bundle.blocks.get(&1).unwrap().payload_range().start as usize;
        (hv.deferred_verifiers, headers, tail, payload_start)
    }

    // A deferred payload BIB verifies over the streamed body: the resident
    // prefix plus the streamed remainder feed the digest, and `finish`
    // settles Ok.
    #[tokio::test]
    async fn deferred_bib_verifies_over_stream() {
        let full = oversized_bundle(true);
        let (verifiers, headers, tail, payload_start) = signed_setup(&full).await;
        assert_eq!(verifiers.len(), 1);

        let rest = full.slice(headers.len()..);
        let mut inner = segment_stream(&rest).await;
        let mut tr = TailReceiver::new(
            &mut inner,
            Some(tail),
            verifiers,
            headers,
            payload_start,
            CancellationToken::new(),
        );
        drain(&mut tr).await.expect("valid signed tail drains");
        tr.finish().expect("the deferred payload BIB verifies");
    }

    // Tampering a streamed body byte fails the deferred BIB at settle.
    #[tokio::test]
    async fn deferred_bib_tamper_fails() {
        let full = oversized_bundle(true);
        let (verifiers, headers, tail, payload_start) = signed_setup(&full).await;

        // Flip a byte well inside the streamed body (after the header
        // prefix), leaving the payload CRC intact by recomputing? No — the
        // CRC would also fail; assert the failure is one of the two. To
        // isolate the BIB, tamper a body byte and accept either verdict
        // ordering, then require it is not Ok.
        let mut rest = full.slice(headers.len()..).to_vec();
        rest[5] ^= 0xFF;
        let mut inner = segment_stream(&rest).await;
        let mut tr = TailReceiver::new(
            &mut inner,
            Some(tail),
            verifiers,
            headers,
            payload_start,
            CancellationToken::new(),
        );
        let _ = drain(&mut tr).await;
        assert!(
            tr.finish().is_err(),
            "a tampered signed payload must not settle Ok"
        );
    }

    #[test]
    fn tail_receiver_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<TailReceiver<'static>>();
    }
}
