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

use alloc::borrow::Cow;
use core::time::Duration;

// Aliased: a bare `Error` in this file is the originator's `services::Error`.
use hardy_bpv7::{
    bpsec::{Error as BpsecError, bcb::OperationSet},
    builder::Builder,
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
    stream::{ConcatError, Receiver},
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
    /// Originate a bundle around an application payload (the ADU door).
    ///
    /// The `Builder` output is valid by construction, so nothing here
    /// parses or validates: the extension fields are read straight off the
    /// built view and the record goes to the shared admission stage as a
    /// resident stream. The id is unique by construction
    /// (`CreationTimestamp::now` is process-monotonic), so
    /// [`DuplicateBundle`](Error::DuplicateBundle) can only mean
    /// a collision with a pre-restart bundle; the caller may resend.
    #[cfg_attr(feature = "instrument", instrument(skip(self, payload)))]
    pub async fn originate(
        &self,
        source: Eid,
        destination: Eid,
        payload: Bytes,
        lifetime: Duration,
        flags: Option<SendOptions>,
    ) -> Result<Id, Error> {
        // Built once, no rebuild-and-retry: `CreationTimestamp::now` issues
        // process-monotonic (time, sequence) pairs, so the id is unique by
        // construction and `DuplicateBundle` can only mean a collision with
        // a pre-restart bundle (a wall clock that stepped backwards across a
        // restart, per RFC 9171 §4.2.7) — surfaced to the caller, who may
        // resend; the store's atomic insert refusal remains the backstop.
        let mut builder = Builder::new(source, destination.clone()).with_lifetime(lifetime);

        // Set flags
        if let Some(flags) = &flags {
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

        let (built, data) = builder
            .with_payload(Cow::Borrowed(&payload))
            .build(CreationTimestamp::now())
            .map_err(|e| Error::Internal(e.into()))?;
        let data = Bytes::from(data);

        let extensions =
            extract_from_built(&built, &data).map_err(|e| Error::Internal(e.into()))?;
        let record = Bundle {
            bpv7: built,
            metadata: BundleMetadata::originated().with_extensions(extensions),
            status: BundleStatus::Dispatching,
        };

        // A zero lifetime expires at creation: refused before the record is
        // admitted, as the raw door's admission gate refuses an expired
        // bundle. The Builder cannot make a hop-exhausted bundle, so lifetime
        // is the one admission check this door needs.
        if record.has_expired() {
            count_originated_dropped(&ReasonCode::LifetimeExpired);
            return Err(Error::Dropped(Some(ReasonCode::LifetimeExpired)));
        }

        // `Bytes` is a `Receiver<Segment>`: the resident build takes the
        // same admission stage as a streamed arrival, with no decorator
        // and no settle-side verdict.
        self.originate_admit(record, data.clone(), data, &HashMap::new(), |_| Ok(()))
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

        // The validating decorator settles the drain verdict — the same
        // machinery as CLA ingress, mapped to the caller's error surface
        // instead of status reports.
        let payload_start = record
            .bpv7
            .blocks
            .get(&1)
            .map_or(headers.len(), |b| b.payload_range().start as usize);
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
    // `finish` at the raw door, nothing at the ADU door — to the caller's
    // error surface, and runs before anything is persisted.
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
        // the duplicate loses its staged data and the door decides whether
        // to retry (the ADU door rebuilds with a fresh timestamp).
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
    // the resident header prefix, then the route lookup — the decision of
    // record, mirroring `decide_at_gate` at CLA ingress. Runs before the
    // payload spools; an `Err` returns with nothing spooled or persisted,
    // so a rejected bundle never awaits its payload.
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
