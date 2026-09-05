//! The originate doors: bundles entering the pipeline from local
//! applications and services.
//!
//! Two doors with one admission stage. The ADU door
//! ([`originate`](Dispatcher::originate)) builds the bundle
//! itself, so it performs no validation — the `Builder` emits valid bundles
//! by construction. The raw door
//! ([`originate_raw`](Dispatcher::originate_raw))
//! is a security boundary: service-provided bytes run the same strict
//! parser and keyed header verification as CLA ingress. Both merge at
//! [`originate_admit`](Dispatcher::originate_admit) — the gate decisions
//! (Originate chain, then the route lookup as the decision of record)
//! settled ahead of the spool, one metadata write, and the routing
//! decision executed as at CLA ingress. Every rejection is a
//! [`services::Error`](Error) to the originator; this path never raises status
//! reports.

use core::time::Duration;

use hardy_async::async_trait;
// Aliased: a bare `Error` in this file is the originator's `services::Error`.
use hardy_bpv7::{
    bpsec::{Error as BpsecError, bcb::OperationSet},
    builder::{Builder, PayloadTrailer, StreamBuild},
    bundle::{Flags, Id},
    creation_timestamp::CreationTimestamp,
    eid::Eid,
    status_report::ReasonCode,
};
use tracing::{debug, error};

#[cfg(feature = "instrument")]
use tracing::instrument;

use super::{
    Dispatcher,
    validate::{ValidatingReceiver, ValidationFailure},
};
use crate::{
    Bytes, HashMap,
    bundle::{
        Bundle, BundleMetadata, BundleStatus,
        parse::{HeaderFailure, HeaderVerify, extract_from_built, parse_headers},
    },
    cla::Segment,
    filter::ChainOutcome,
    otel_metrics::{reason_label, status_label},
    routing::{Decision, DispatchAction},
    services::{Error, SendOptions},
    stream::{ConcatError, Receiver, RecvError},
};

/// Count an origination refused with a reason code, labelled by it. Every
/// such refusal at the originate doors increments this counter exactly once;
/// a new refusal site calls this rather than respelling the counter name and
/// label vocabulary.
fn count_originated_dropped(reason: &ReasonCode) {
    metrics::counter!("bpa.bundle.originated.dropped", "reason" => reason_label(reason))
        .increment(1);
}

impl Dispatcher {
    /// Originate a bundle around a resident application payload (the ADU
    /// door).
    ///
    /// Pure sugar over [`originate_streamed`](Self::originate_streamed): a
    /// resident payload is a one-segment stream (`Bytes` is a
    /// `Receiver<Segment>`), and its length is the declaration.
    #[cfg_attr(feature = "instrument", instrument(skip(self, payload)))]
    pub async fn originate(
        &self,
        source: Eid,
        destination: Eid,
        mut payload: Bytes,
        lifetime: Duration,
        flags: Option<SendOptions>,
    ) -> Result<Id, Error> {
        let total_len = payload.len() as u64;
        self.originate_streamed(
            source,
            destination,
            total_len,
            &mut payload,
            lifetime,
            flags,
        )
        .await
    }

    /// Originate a bundle around an application payload supplied as a
    /// segment stream (the streamed ADU door).
    ///
    /// The stream must deliver exactly `total_len` payload bytes — the
    /// declaration frames the wire form before the first pull — and an
    /// over- or under-delivering producer is rejected
    /// ([`PayloadTooLarge`](Error::PayloadTooLarge) /
    /// [`PayloadUnderrun`](Error::PayloadUnderrun)) with nothing
    /// persisted. The bundle id is unique by construction —
    /// `CreationTimestamp::now` issues process-monotonic `(time, sequence)`
    /// pairs — so
    /// [`DuplicateBundle`](Error::DuplicateBundle) can only mean
    /// a collision with a pre-restart bundle (a wall clock that stepped
    /// backwards across a restart); the caller may resend.
    #[cfg_attr(feature = "instrument", instrument(skip(self, stream)))]
    pub async fn originate_streamed(
        &self,
        source: Eid,
        destination: Eid,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
        lifetime: Duration,
        flags: Option<SendOptions>,
    ) -> Result<Id, Error> {
        // The caller's declaration is bounded before anything is built: a
        // payload past `max_bundle_size` is refused with its declared length,
        // and no accepted length can overflow the built bundle's extents.
        let max = self.max_bundle_size_wire();
        if total_len > max {
            debug!(
                "Originated payload declares {total_len} bytes, exceeding max_bundle_size {max}"
            );
            return Err(Error::PayloadTooLarge {
                size: total_len,
                max,
            });
        }

        let built = self
            .originate_builder(source, destination, lifetime, &flags)
            .build_stream(total_len, CreationTimestamp::now())
            .map_err(|e| Error::Internal(e.into()))?;
        self.admit_built(built, stream, total_len).await
    }

    // `originate_streamed`'s Builder setup: flags and the report-to
    // endpoint, from the caller's `SendOptions`.
    fn originate_builder(
        &self,
        source: Eid,
        destination: Eid,
        lifetime: Duration,
        flags: &Option<SendOptions>,
    ) -> Builder<'static> {
        let mut builder = Builder::new(source, destination.clone()).with_lifetime(lifetime);
        if let Some(flags) = flags {
            builder = builder.with_flags(Flags {
                do_not_fragment: flags.do_not_fragment,
                app_ack_requested: flags.request_ack,
                report_status_time: flags.report_status_time,
                receipt_report_requested: flags.notify_reception,
                forward_report_requested: flags.notify_forwarding,
                delivery_report_requested: flags.notify_delivery,
                delete_report_requested: flags.notify_deletion,
                ..Default::default()
            });

            if flags.notify_reception
                || flags.notify_forwarding
                || flags.notify_delivery
                || flags.notify_deletion
            {
                builder = builder.with_report_to(self.node_ids.get_admin_endpoint(&destination));
            }
        }
        builder
    }

    // `originate_streamed`'s admission: wrap the streamed build and the
    // payload source in a `BuiltReceiver` — the CRC applied at the point of
    // origination as the payload flows — and run the shared stage, mapping
    // a declared-length violation to the caller's error surface.
    async fn admit_built(
        &self,
        built: StreamBuild,
        stream: &mut dyn Receiver<Segment>,
        declared: u64,
    ) -> Result<Id, Error> {
        let StreamBuild {
            bundle: built_view,
            prefix,
            trailer,
        } = built;

        let prefix = Bytes::from(prefix);
        let extracted =
            extract_from_built(&built_view, &prefix).map_err(|e| Error::Internal(e.into()))?;
        let mut metadata = BundleMetadata::originated();
        metadata.extensions = extracted;
        let record = Bundle {
            bpv7: built_view,
            metadata,
            status: BundleStatus::Dispatching,
        };

        // A zero lifetime expires at creation: refused before a payload byte
        // is pulled, as the raw door's admission gate refuses an expired
        // bundle. The Builder cannot make a hop-exhausted bundle, so lifetime
        // is the one admission check this door needs.
        if record.has_expired() {
            count_originated_dropped(&ReasonCode::LifetimeExpired);
            return Err(Error::Dropped(Some(ReasonCode::LifetimeExpired)));
        }

        // The Originate chain's payload peek, held before the gate as the raw
        // door's header pass holds it: the app's first payload bytes join the
        // prefix the gate reads. A bundle the shared stage refuses by size
        // pulls nothing, and an Originate chain with no peek declared holds nothing.
        let admissible = record.bpv7.encoded_len() <= self.max_bundle_size_wire();
        let peek = if admissible {
            self.filters.originate.peek
        } else {
            0
        };
        let mut built_rx = BuiltReceiver::new(prefix, stream, trailer, declared);
        let head = match built_rx.hold(peek).await {
            Ok(head) => head,
            Err(Some(violation)) => return Err(violation.into_error(declared)),
            Err(None) => return Err(Error::StreamCancelled),
        };
        self.originate_admit(record, head, built_rx, &HashMap::new(), |rx| {
            rx.into_violation()
                .map_or(Ok(()), |violation| Err(violation.into_error(declared)))
        })
        .await
    }

    /// Originate a service-built bundle from a segment stream (the raw
    /// door, for the low-level `Service` trait).
    ///
    /// This is a security boundary — services are not trusted — so the raw
    /// door carries all the validation the ADU door doesn't: the stream
    /// runs the same strict parser and keyed header verification as CLA
    /// ingress (non-canonical input is rejected, never rewritten; the
    /// bytes are stored and forwarded as received), the source must match
    /// the registered endpoint, and the lifetime/hop admission and
    /// config-gated RFC 9171 validity checks apply at the same pre-drain
    /// seat. A producer that goes away before the final segment cancels
    /// the send: nothing is stored, and the caller gets
    /// [`StreamCancelled`](Error::StreamCancelled).
    #[cfg_attr(feature = "instrument", instrument(skip(self, stream)))]
    pub async fn originate_raw(
        &self,
        expected_source: &Eid,
        stream: &mut dyn Receiver<Segment>,
    ) -> Result<Id, Error> {
        // The pass holds the Originate chain's payload peek: none for an
        // empty chain.
        let (hv, headers, tail, bcb_ops) = match parse_headers(
            stream,
            self.max_bundle_size_wire(),
            self.filters.originate.peek,
            self.key_provider(),
        )
        .await
        {
            Ok(parts) => parts,
            Err(HeaderFailure::Cancelled) => {
                return Err(Error::StreamCancelled);
            }
            Err(HeaderFailure::TooLarge { size, max }) => {
                return Err(Error::PayloadTooLarge { size, max });
            }
            // The report half is CLA-ingress machinery; an originator
            // gets the parse or verification error itself, counted under
            // the reason a report would carry.
            Err(HeaderFailure::Invalid { error, report }) => {
                count_originated_dropped(
                    &report.map_or(ReasonCode::BlockUnintelligible, |(_, _, reason)| reason),
                );
                return Err(Error::InvalidBundle(error));
            }
        };

        // Verify source matches the registered service endpoint
        // (registration already validated that the EID belongs to our node).
        if hv.bundle.primary.id.source != *expected_source {
            return Err(Error::InvalidDestination(
                hv.bundle.primary.id.source.clone(),
            ));
        }

        let mut metadata = BundleMetadata::originated();

        // Lifetime/hop admission and the config-gated RFC 9171 validity
        // checks, at the same pre-drain seat as CLA ingress: parse-validated
        // service bytes can trip any of them, where the ADU door, whose
        // Builder cannot make a hop-exhausted bundle, checks lifetime alone.
        // Nothing is stored yet — a rejection is purely an error to the
        // caller.
        if let Some(reason) = hv.gate_reason(metadata.received_at()) {
            count_originated_dropped(&reason);
            return Err(Error::Dropped(Some(reason)));
        }
        if let Some(reason) = self.rfc9171_gate_reason(&hv) {
            count_originated_dropped(&reason);
            return Err(Error::Dropped(Some(reason)));
        }

        let HeaderVerify {
            bundle,
            extensions,
            to_remove,
            // Originated bundles never raise reception reports — reception
            // is a CLA-ingress notion; the originator learns the outcome
            // from this call's return value.
            report: _,
            deferred_verifiers,
        } = hv;
        metadata.extensions = extensions;

        // The §E removals travel with the bundle to the output doors,
        // sorted for a deterministic persisted order, exactly as at CLA
        // ingress — the bundle is stored as received.
        let mut to_remove: Vec<u64> = to_remove.into_iter().collect();
        to_remove.sort_unstable();
        metadata.to_remove = to_remove;

        let record = Bundle {
            bpv7: bundle,
            metadata,
            status: BundleStatus::Dispatching,
        };

        // Early duplicate probe, before the decorator and the gate
        // decisions: raw bytes are externally shaped, so a replay here is
        // incoming-duplicate traffic exactly as at CLA ingress, settled for
        // the price of a cache lookup without consuming the caller's
        // stream. The ADU doors carry no probe — their ids are freshly
        // generated, and a collision settles at insert_metadata's atomic
        // refusal. Advisory only: copies racing in concurrently, ids past
        // the cache's horizon, and a cold cache after restart all settle
        // at that same refusal.
        if self.store.seen_recently(record.id()) {
            debug!("Duplicate bundle detected at the originate gate");
            return Err(Error::DuplicateBundle);
        }

        // The validating decorator settles the drain verdict — the same
        // machinery as CLA ingress, mapped to the caller's error surface
        // instead of status reports.
        let payload_start = record.bpv7.blocks.get(&1).map_or(headers.len(), |b| {
            usize::try_from(b.payload_range().start)
                .expect("parse bounds pre-payload offsets to usize")
        });
        let tail_rx = ValidatingReceiver::new(
            stream,
            tail,
            deferred_verifiers,
            headers.clone(),
            payload_start,
        );
        self.originate_admit(record, headers, tail_rx, &bcb_ops, |rx| {
            rx.finish().map_err(|failure| {
                if let Some(reason) = failure.reason_code() {
                    count_originated_dropped(&reason);
                }
                match failure {
                    // The transfer never completed; the producer may retry.
                    ValidationFailure::Truncated => Error::StreamCancelled,
                    ValidationFailure::Invalid(e) => Error::InvalidBundle(e),
                    ValidationFailure::IntegrityFailed { .. } => {
                        Error::InvalidBundle(BpsecError::IntegrityCheckFailed.into())
                    }
                }
            })
        })
        .await
    }

    // The shared admission stage where the originate doors merge: the gate
    // decisions (Originate chain, then the route lookup — the decision of
    // record) settle first, the spool follows, the door's stream verdict
    // settles against the save, and the admitted record is written once and
    // its routing decision executed through `execute_admitted`. Originated
    // bundles do not transit the dispatch queue (`DispatchPending` belongs to
    // the re-dispatch paths) beyond its admin-endpoint and fragment arms,
    // mirroring fresh CLA arrivals; a crash between the insert and the
    // execution recovers through restart's `Dispatching` arm.
    //
    // `stream` is the door's whole-bundle byte source (`head` included);
    // `settle` maps the door's post-drain verdict — a `ValidatingReceiver`'s
    // `finish` at the raw door, a `BuiltReceiver`'s length verdict at the
    // ADU door — to the caller's error surface, and runs before anything is
    // persisted.
    async fn originate_admit<S, F>(
        &self,
        bundle: Bundle,
        head: Bytes,
        mut stream: S,
        bcbs: &HashMap<u64, OperationSet>,
        settle: F,
    ) -> Result<Id, Error>
    where
        S: Receiver<Segment>,
        F: FnOnce(S) -> Result<(), Error>,
    {
        // The declared wire size bounds both doors before a byte spools.
        // The comparison stays in u64: the declaration may exceed this
        // target's address space.
        let declared = bundle.bpv7.encoded_len();
        let max = self.max_bundle_size_wire();
        if declared > max {
            debug!("Originated bundle declares {declared} bytes, exceeding max_bundle_size {max}");
            return Err(Error::PayloadTooLarge {
                size: declared,
                max,
            });
        }

        // The gate decisions settle before a byte spools, so a rejection
        // returns here with nothing spooled, never awaiting the payload.
        let (mut bundle, decision) = self.decide_at_originate_gate(bundle, head, bcbs)?;

        // Drain the whole wire form into the store, bounded by
        // `max_bundle_size` as the defensive backstop.
        let max_size = self.max_bundle_size_mem();
        let outcome = self.store.save_stream(&mut stream, max_size).await;

        // Settle the door's stream verdict against the save: a save the
        // verdict rejects was staged before the verdict settled — the
        // discard half of the streaming contract.
        let (storage_name, data_len) = match settle(stream) {
            Ok(()) => match outcome {
                Ok(save) => save,
                // The door's decorator saw a clean stream, so a drain
                // failure is the transport's: the producer went away. The
                // drain's defensive bound cannot trip here: the declared
                // wire size was bounded before the gate, and both doors'
                // decorators end the stream at the declaration.
                Err(ConcatError::Cancelled) => return Err(Error::StreamCancelled),
                Err(ConcatError::TooLarge { size, max }) => {
                    return Err(Error::PayloadTooLarge {
                        size: size as u64,
                        max: max as u64,
                    });
                }
            },
            Err(e) => {
                if let Ok((storage_name, _)) = outcome {
                    self.store.delete_data(&storage_name).await;
                }
                return Err(e);
            }
        };
        bundle.metadata.storage_name = Some(storage_name);

        // `insert_metadata` is the authoritative atomic duplicate check;
        // the duplicate loses its staged data and `DuplicateBundle`
        // surfaces to the door's caller.
        if !self.store.insert_metadata(&bundle).await {
            if let Some(storage_name) = &bundle.metadata.storage_name {
                self.store.delete_data(storage_name).await;
            }
            return Err(Error::DuplicateBundle);
        }

        metrics::counter!("bpa.bundle.originated").increment(1);
        metrics::counter!("bpa.bundle.originated.bytes").increment(data_len as u64);

        let bundle_id = bundle.id().clone();
        metrics::gauge!("bpa.bundle.status", "state" => status_label(&bundle.status))
            .increment(1.0);

        // Execute the routing decision of record as at CLA ingress: decided
        // again if the table changed during the drain, with the
        // admin-endpoint and fragment arms queued for the processing pool.
        self.execute_admitted(bundle, decision).await;
        Ok(bundle_id)
    }

    // The gate decisions for an originated record: the Originate chain on
    // the resident prefix (the headers and any held peek), then the route
    // lookup — the decision of record, mirroring `decide_at_gate` at CLA
    // ingress. Runs before the payload spools; an `Err` returns with nothing
    // spooled or persisted, so a rejected bundle never awaits its payload
    // past the peek.
    fn decide_at_originate_gate(
        &self,
        bundle: Bundle,
        head: Bytes,
        bcbs: &HashMap<u64, OperationSet>,
    ) -> Result<(Bundle, Decision), Error> {
        // The Originate chain runs synchronously on the record; a
        // Classifier's metadata deltas survive into the persisted record. A
        // filter reading a not-yet-resident payload gets the reader's
        // not-resident `None`.
        let bundle = if self.filters.has_originate() {
            match self
                .filters
                .run_originate(bundle, head, bcbs, &*self.key_provider)
            {
                Ok(ChainOutcome::Continue(bundle, _)) => bundle,
                Ok(ChainOutcome::Drop(_, reason)) => {
                    let label = reason.unwrap_or(ReasonCode::NoAdditionalInformation);
                    count_originated_dropped(&label);
                    debug!("Originate filter chain dropped the bundle: {label:?}");
                    return Err(Error::Dropped(reason));
                }
                Err((_, e)) => {
                    error!("Originate filter chain failed: {e}");
                    return Err(Error::Internal(e));
                }
            }
        } else {
            bundle
        };

        // Route at the door — the decision of record for this origination,
        // carrying the table it was made against: the commit decides again
        // if the table changed during the drain. An explicit Drop route
        // rejects here — doomed traffic is never persisted — and the
        // originator always gets an error: even a reasonless (silent) Drop
        // surfaces as `Dropped(None)`, where the CLA gate drops silently.
        let decision = self.rib.decide(&bundle);
        if let Some(DispatchAction::Drop(reason)) = decision.action {
            let label = reason.unwrap_or(ReasonCode::NoAdditionalInformation);
            count_originated_dropped(&label);
            debug!("Route lookup drops the bundle at the originate door: {label:?}");
            return Err(Error::Dropped(reason));
        }

        Ok((bundle, decision))
    }
}

// Why a [`BuiltReceiver`]'s stream ended early: the caller's payload did
// not honour the declared length. Recorded rather than smuggled through
// `RecvError`, so the door maps it to the caller's error surface after the
// spool settles.
#[derive(Debug)]
enum LengthViolation {
    // The payload ran past the declaration; `size` is the count at the
    // first excess byte.
    Overrun { size: u64 },
    // The payload completed short of the declaration.
    Underrun { size: u64 },
}

impl LengthViolation {
    // The caller's error for the violation of a `declared`-byte payload.
    fn into_error(self, declared: u64) -> Error {
        match self {
            LengthViolation::Overrun { size } => Error::PayloadTooLarge {
                size,
                max: declared,
            },
            LengthViolation::Underrun { size } => Error::PayloadUnderrun {
                size,
                expected: declared,
            },
        }
    }
}

// The build-side twin of the ingress door's validating decorator: presents
// a [`Builder::build_stream`] output — the wire prefix, the caller's
// payload stream, and the trailer continuation — as one
// `Receiver<Segment>` carrying the whole wire form, ready to drain through
// `Store::save_stream`. The payload block's CRC is applied at the point of
// origination: each payload run feeds the [`PayloadTrailer`] digest as it
// flows, and the trailer's bytes (the CRC field, then the outer break) are
// yielded as the stream's `Final` segment.
//
// The declared length is enforced as the bytes flow — the prefix framed the
// payload byte string with it, so a violating stream must never reach the
// store. The trailer owns the count: it refuses a run past the declaration
// and, finished at the inner stream's `Final`, a payload short of it. A
// violation ends the stream (`RecvError`, surfacing downstream as an
// incomplete drain) and is recorded for `into_violation`; a producer that
// goes away early ends the stream without a violation, exactly as any
// truncated transfer.
struct BuiltReceiver<'a> {
    inner: &'a mut dyn Receiver<Segment>,
    // The resident wire prefix, yielded first; `None` once yielded.
    prefix: Option<Bytes>,
    // The emit-side continuation; `None` once the inner stream's `Final` has
    // been absorbed and the trailer finished: nothing further is pulled.
    trailer: Option<PayloadTrailer>,
    // The finished trailer's bytes, yielded as the stream's `Final`; `None`
    // until the inner `Final`, and once yielded.
    closing: Option<Bytes>,
    declared: u64,
    violation: Option<LengthViolation>,
}

impl<'a> BuiltReceiver<'a> {
    fn new(
        prefix: Bytes,
        inner: &'a mut dyn Receiver<Segment>,
        trailer: PayloadTrailer,
        declared: u64,
    ) -> Self {
        Self {
            inner,
            prefix: Some(prefix),
            trailer: Some(trailer),
            closing: None,
            declared,
            violation: None,
        }
    }

    // The declared-length verdict, settled after the drain: `None` is a
    // clean pass (or a plain truncation, which the transport surfaces
    // itself).
    fn into_violation(self) -> Option<LengthViolation> {
        self.violation
    }

    // Holds the payload peek: pulls the caller's stream until the payload's
    // first `peek` bytes (all of it, if shorter) have arrived, each absorbed
    // as `recv` absorbs it, and joins them to the prefix, which is still
    // yielded first. Returns the resident head the gate reads. A violation
    // ends the hold with it, and a producer gone away with `None`.
    async fn hold(&mut self, peek: usize) -> Result<Bytes, Option<LengthViolation>> {
        let prefix = self.prefix.take().unwrap_or_default();
        let target = (peek as u64).min(self.declared);
        let pending = |rx: &Self| {
            rx.trailer
                .as_ref()
                .is_some_and(|trailer| trailer.received() < target)
        };
        if !pending(self) {
            self.prefix = Some(prefix.clone());
            return Ok(prefix);
        }
        let mut head = prefix.to_vec();
        while pending(self) {
            let segment = self.inner.recv().await.map_err(|_| None)?;
            head.extend_from_slice(&self.absorb(segment).map_err(Some)?);
        }
        let head = Bytes::from(head);
        self.prefix = Some(head.clone());
        Ok(head)
    }

    // Absorbs one segment of the caller's stream into the trailer, which
    // checks the declared length and feeds its digest; the inner `Final`
    // finishes it. Returns the payload bytes.
    fn absorb(&mut self, segment: Segment) -> Result<Bytes, LengthViolation> {
        let (bytes, last) = match segment {
            Segment::Next(bytes) => (bytes, false),
            Segment::Final(bytes) => (bytes, true),
        };
        if let Some(trailer) = &mut self.trailer {
            trailer
                .update(&bytes)
                .map_err(|overrun| LengthViolation::Overrun {
                    size: overrun.received,
                })?;
        }
        if last && let Some(trailer) = self.trailer.take() {
            let closing = trailer
                .finish()
                .map_err(|underrun| LengthViolation::Underrun {
                    size: underrun.received,
                })?;
            self.closing = Some(Bytes::from(closing));
        }
        Ok(bytes)
    }
}

#[async_trait]
impl Receiver<Segment> for BuiltReceiver<'_> {
    async fn recv(&mut self) -> Result<Segment, RecvError> {
        // A violation is terminal: never yield more bytes downstream.
        if self.violation.is_some() {
            return Err(RecvError);
        }
        // The wire prefix goes first.
        if let Some(prefix) = self.prefix.take() {
            return Ok(Segment::Next(prefix));
        }
        // The payload is complete: the trailer's bytes terminate the wire
        // form, once.
        if self.trailer.is_none() {
            return self.closing.take().map(Segment::Final).ok_or(RecvError);
        }

        let segment = self.inner.recv().await?;
        match self.absorb(segment) {
            // Inner `Final` is re-tagged: the trailer is the stream's real
            // end.
            Ok(bytes) => Ok(Segment::Next(bytes)),
            Err(violation) => {
                self.violation = Some(violation);
                Err(RecvError)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // Aliased: `Receiver` is the BPA's stream trait, from the module above.
    use hardy_async::channel::{Receiver as ChannelReceiver, bounded};

    use super::*;
    use crate::BytesMut;

    const PAYLOAD: usize = 5000;
    const CHUNK: usize = 700;

    fn source() -> Eid {
        "ipn:1.2".parse().unwrap()
    }

    fn destination() -> Eid {
        "ipn:2.1".parse().unwrap()
    }

    // Feed `bytes` into a bounded channel as segments (last one `Final`),
    // the payload producer a `BuiltReceiver` wraps.
    async fn segment_stream(bytes: &[u8]) -> ChannelReceiver<Segment> {
        let chunks: Vec<&[u8]> = bytes.chunks(CHUNK).collect();
        let (tx, rx) = bounded(chunks.len().max(1));
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

    // Drain a receiver to completion, returning the concatenated bytes.
    async fn drain(rx: &mut impl Receiver<Segment>) -> Result<Bytes, RecvError> {
        let mut out = BytesMut::new();
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

    // The receiver carries the whole wire form — prefix, payload, trailer —
    // byte-identical to the resident build for the same timestamp: the CRC
    // applied as the payload streamed is the CRC `build()` would have
    // written.
    #[tokio::test]
    async fn built_receiver_carries_the_whole_wire_form() {
        let payload = vec![0xC3_u8; PAYLOAD];
        let timestamp = CreationTimestamp::now();
        let (_, resident) = Builder::new(source(), destination())
            .with_payload(payload.as_slice().into())
            .build(timestamp.clone())
            .expect("build");
        let built = Builder::new(source(), destination())
            .build_stream(payload.len() as u64, timestamp)
            .expect("build_stream");

        let mut inner = segment_stream(&payload).await;
        let mut rx = BuiltReceiver::new(
            Bytes::from(built.prefix),
            &mut inner,
            built.trailer,
            payload.len() as u64,
        );
        let yielded = drain(&mut rx).await.expect("clean stream drains");
        assert_eq!(yielded.as_ref(), resident.as_ref());
        assert!(rx.into_violation().is_none());
    }

    // A held peek joins the prefix and is yielded with it: the gate reads
    // the payload's first bytes, and the drain still carries the whole wire
    // form, byte-identical to the resident build. Holding pulls only the
    // segments the peek needs.
    #[tokio::test]
    async fn a_held_peek_leads_the_whole_wire_form() {
        let payload: Vec<u8> = (0..PAYLOAD as u32).map(|i| i as u8).collect();
        let timestamp = CreationTimestamp::now();
        let (_, resident) = Builder::new(source(), destination())
            .with_payload(payload.as_slice().into())
            .build(timestamp.clone())
            .expect("build");
        for (peek, held) in [(0, 0), (1000, 2 * CHUNK), (2 * PAYLOAD, PAYLOAD)] {
            let built = Builder::new(source(), destination())
                .build_stream(payload.len() as u64, timestamp.clone())
                .expect("build_stream");
            let prefix_len = built.prefix.len();
            let mut inner = segment_stream(&payload).await;
            let mut rx = BuiltReceiver::new(
                Bytes::from(built.prefix),
                &mut inner,
                built.trailer,
                payload.len() as u64,
            );
            let head = rx.hold(peek).await.expect("a clean hold");
            assert_eq!(
                head.len(),
                prefix_len + held,
                "peek {peek} holds {held} bytes"
            );
            assert_eq!(head[prefix_len..], payload[..held]);
            let yielded = drain(&mut rx).await.expect("clean stream drains");
            assert_eq!(yielded.as_ref(), resident.as_ref(), "peek {peek}");
            assert!(rx.into_violation().is_none());
        }
    }

    // A violation inside the hold — an overrun, or an underrun when the
    // `Final` lands inside the peek — ends it with the violation; a producer
    // gone away ends it with none.
    #[tokio::test]
    async fn a_hold_reports_a_violation_or_a_cancel() {
        let payload = vec![0xC3_u8; PAYLOAD];
        let build = || {
            Builder::new(source(), destination())
                .build_stream(100, CreationTimestamp::now())
                .expect("build_stream")
        };
        let built = build();
        let mut inner = segment_stream(&payload).await;
        let mut rx = BuiltReceiver::new(Bytes::from(built.prefix), &mut inner, built.trailer, 100);
        assert!(matches!(
            rx.hold(50).await,
            Err(Some(LengthViolation::Overrun { size })) if size == CHUNK as u64
        ));

        // The `Final` lands inside the peek, short of the declaration.
        let built = build();
        let mut inner = segment_stream(&payload[..30]).await;
        let mut rx = BuiltReceiver::new(Bytes::from(built.prefix), &mut inner, built.trailer, 100);
        assert!(matches!(
            rx.hold(50).await,
            Err(Some(LengthViolation::Underrun { size: 30 }))
        ));

        let built = build();
        let (tx, mut inner) = bounded::<Segment>(1);
        drop(tx);
        let mut rx = BuiltReceiver::new(Bytes::from(built.prefix), &mut inner, built.trailer, 100);
        assert!(matches!(rx.hold(50).await, Err(None)));
    }

    // A zero-length declaration requires an empty `Final`, as any declared
    // length requires exactly that many bytes: the prefix and the trailer
    // are then the whole wire form, and payload bytes past the declaration
    // are an overrun.
    #[tokio::test]
    async fn built_receiver_zero_length_requires_an_empty_final() {
        let timestamp = CreationTimestamp::now();
        let (_, resident) = Builder::new(source(), destination())
            .with_payload(b"".as_slice().into())
            .build(timestamp.clone())
            .expect("build");
        let built = Builder::new(source(), destination())
            .build_stream(0, timestamp)
            .expect("build_stream");
        let (tx, mut inner) = bounded(1);
        tx.send(Segment::Final(Bytes::new()))
            .await
            .expect("channel open");
        let mut rx = BuiltReceiver::new(Bytes::from(built.prefix), &mut inner, built.trailer, 0);
        let yielded = drain(&mut rx).await.expect("prefix + trailer drain");
        assert_eq!(yielded.as_ref(), resident.as_ref());
        assert!(rx.into_violation().is_none());

        let built = Builder::new(source(), destination())
            .build_stream(0, CreationTimestamp::now())
            .expect("build_stream");
        let mut inner = segment_stream(b"abc").await;
        let mut rx = BuiltReceiver::new(Bytes::from(built.prefix), &mut inner, built.trailer, 0);
        assert!(matches!(drain(&mut rx).await, Err(RecvError)));
        assert!(matches!(
            rx.into_violation(),
            Some(LengthViolation::Overrun { size: 3 })
        ));
    }

    // A producer running past the declaration is a recorded overrun: the
    // stream ends before the excess reaches downstream.
    #[tokio::test]
    async fn built_receiver_records_an_overrun() {
        let payload = vec![0xC3_u8; PAYLOAD];
        let built = Builder::new(source(), destination())
            .build_stream(100, CreationTimestamp::now())
            .expect("build_stream");

        let mut inner = segment_stream(&payload).await;
        let mut rx = BuiltReceiver::new(Bytes::from(built.prefix), &mut inner, built.trailer, 100);
        assert!(
            matches!(drain(&mut rx).await, Err(RecvError)),
            "the overrun ends the stream"
        );
        assert!(matches!(
            rx.into_violation(),
            Some(LengthViolation::Overrun { size }) if size == CHUNK as u64
        ));
    }

    // A producer completing short of the declaration is a recorded
    // underrun.
    #[tokio::test]
    async fn built_receiver_records_an_underrun() {
        let payload = vec![0xC3_u8; 100];
        let built = Builder::new(source(), destination())
            .build_stream(PAYLOAD as u64, CreationTimestamp::now())
            .expect("build_stream");

        let mut inner = segment_stream(&payload).await;
        let mut rx = BuiltReceiver::new(
            Bytes::from(built.prefix),
            &mut inner,
            built.trailer,
            PAYLOAD as u64,
        );
        assert!(
            matches!(drain(&mut rx).await, Err(RecvError)),
            "the underrun ends the stream"
        );
        assert!(matches!(
            rx.into_violation(),
            Some(LengthViolation::Underrun { size: 100 })
        ));
    }

    // A producer that goes away mid-payload ends the stream with no
    // violation: a plain truncation, the transport's to surface.
    #[tokio::test]
    async fn built_receiver_truncation_is_not_a_violation() {
        let payload = vec![0xC3_u8; PAYLOAD];
        let built = Builder::new(source(), destination())
            .build_stream(payload.len() as u64, CreationTimestamp::now())
            .expect("build_stream");

        // One chunk, then the producer drops without `Final`.
        let (tx, mut inner) = bounded(1);
        tx.send(Segment::Next(Bytes::copy_from_slice(&payload[..CHUNK])))
            .await
            .expect("channel open");
        drop(tx);

        let mut rx = BuiltReceiver::new(
            Bytes::from(built.prefix),
            &mut inner,
            built.trailer,
            payload.len() as u64,
        );
        assert!(
            matches!(drain(&mut rx).await, Err(RecvError)),
            "the truncation ends the stream"
        );
        assert!(rx.into_violation().is_none());
    }
}
