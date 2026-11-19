use alloc::sync::Arc;

use hardy_bpv7::{
    block::BibCoverage, bpsec::bcb::OperationSet, crc::CrcType, eid::NodeId,
    status_report::ReasonCode,
};
use tracing::debug;

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
        parse::{HeaderFailure, HeaderVerify, ReceptionReport, parse_headers},
    },
    cla::{Acceptance, Address, Segment},
    filter::ChainOutcome,
    otel_metrics::{reason_label, status_label},
    routing::{Decision, DispatchAction},
    stream::{ConcatError, Receiver},
};

/// Counts a drop of a received bundle, labelled by reason.
///
/// Every drop site in the received pipeline increments this counter exactly
/// once (see the drop accounting note on `receive_bundle`). A new drop site
/// calls this rather than respelling the counter name and label vocabulary.
fn count_received_dropped(reason: &ReasonCode) {
    metrics::counter!("bpa.bundle.received.dropped", "reason" => reason_label(reason)).increment(1);
}

// An arrival that `decide_at_gate` admitted. It returns `None` for a
// rejection, which it has already counted and reported.
struct Admitted {
    bundle: Bundle,
    // The routing decision of record, with the table it was made against.
    decision: Decision,
}

// The outcome of the shared receive pipeline, for its three callers.
//
// `Dispatched` and `Disposed` are both acceptance: the bundle ran the
// Ingress chain and its routing decision was executed or queued for
// dispatch, or it was dropped internally with reports. `Refused` is the one
// non-acceptance outcome, where the transfer could not be taken at all,
// through truncation or the size cap, and its custodian keeps
// responsibility.
pub(super) enum Received {
    /// Admitted: the bundle ran the Ingress chain, was written once to the
    /// metadata store, and had the gate's routing decision executed (or
    /// queued for dispatch).
    Dispatched,
    /// Accepted and disposed of internally, as invalid, gate-rejected,
    /// chain-dropped or duplicate, with reports emitted where possible and
    /// nothing dispatched.
    Disposed,
    /// Acceptance refused: truncated stream or over the size cap. The
    /// refusal site logs the specifics.
    Refused,
}

impl Dispatcher {
    // Entry point for bundles received from CLAs.
    //
    // The return value is the acceptance verdict of the `cla::Sink::dispatch`
    // contract. `Accepted` covers every internally-disposed case, invalid,
    // gate-rejected or duplicate, as well: refusing an invalid bundle would
    // invite the previous node's custody and retransmission machinery to
    // resend identical bytes forever, so the first BPA to detect a content
    // problem accepts and terminates the bundle. `Refused` is reserved for
    // transfers this node could not take at all, through truncation or the
    // size cap. Returning without draining `stream` means nothing on its
    // own: the producer learns the verdict from this return, never from the
    // stream closing.
    //
    // # Bundle State
    //
    // `process_received_bundle` runs the Ingress filter and the gate route
    // lookup, writes the record once at `Dispatching`, and executes the
    // routing decision through `execute_admitted`. Fresh arrivals do not
    // transit the dispatch queue, `DispatchPending` belonging to the
    // re-dispatch paths, except the admin-endpoint and fragment arms, which
    // queue for the processing pool.
    //
    // See [Bundle State Machine Design](../../docs/bundle_state_machine_design.md)
    // for the complete state transition diagram.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn receive_bundle(
        &self,
        ingress_cla: Arc<str>,
        ingress_peer_node: Option<&NodeId>,
        ingress_peer_addr: Option<&Address>,
        stream: &mut dyn Receiver<Segment>,
    ) -> Acceptance {
        let metadata = BundleMetadata::ingress(
            ingress_cla,
            ingress_peer_node.cloned(),
            ingress_peer_addr.cloned(),
        );

        // Truncation and the size bound refuse the transfer, so that the
        // CLA withholds its acknowledgement and the peer retransmits, while
        // bundles that are invalid but complete are accepted and handled
        // internally: the CLA cannot fix invalid content, and the transfer
        // itself succeeded.
        //
        // Drop sites inside `process_received_bundle` count themselves under
        // `bpa.bundle.received.dropped` with a `reason` label. Nothing was
        // stored on the CLA path before a drop, so there is no data to clean
        // up.
        match self.process_received_bundle(stream, metadata).await {
            Received::Dispatched | Received::Disposed => Acceptance::Accepted,
            Received::Refused => Acceptance::Refused,
        }
    }

    // Shared bundle processing: parses and validates the headers, gates
    // them through the Ingress chain and the route lookup, drains and stores
    // the payload, reports, and executes the routing decision.
    //
    // Called from the CLA ingress path, `receive_bundle`, the ADU reassembly
    // path, `reassemble`, and restart orphan recovery. Bundle validation is
    // handled internally: an invalid bundle is logged, counted, and dropped
    // with status reports where possible, as `Disposed`, and only truncation
    // and the size cap refuse, as `Refused`. An admitted bundle runs the
    // Ingress chain, is written once to the metadata store, the P1
    // checkpoint, and has its gate routing decision executed or queued for
    // dispatch, as `Dispatched`.
    //
    // Every caller hands the bundle as a segment stream, one holding whole
    // bytes passing them directly, `Bytes` being a `Receiver<Segment>`. The
    // spool saves an admitted bundle fresh, so `metadata.storage_name` must
    // be unset, and a caller replaying pre-stored bytes, from reassembly or
    // restart orphans, owns its own copy's cleanup after this returns.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub(super) async fn process_received_bundle(
        &self,
        stream: &mut dyn Receiver<Segment>,
        mut metadata: BundleMetadata,
    ) -> Received {
        // The header pass parses the header chain off the stream and runs
        // keyed header verification, both in `bundle::parse`, before a
        // payload still arriving is spooled. `Err` carries an optional
        // recoverable bundle to report before dropping, the reporting
        // machinery belonging here; a structural failure, or a stream that
        // ended inside the headers, carries none. The pass holds the Ingress
        // chain's payload peek, none for an empty chain.
        let (hv, headers, tail, bcb_ops) = match parse_headers(
            stream,
            self.max_bundle_size_wire(),
            self.filters.ingress.peek,
            self.key_provider(),
        )
        .await
        {
            Ok(parts) => parts,
            Err(HeaderFailure::Cancelled) => {
                debug!("Bundle stream cancelled mid-header; refused");
                return Received::Refused;
            }
            Err(HeaderFailure::TooLarge { size, max }) => {
                // Covers both bounds `parse_headers` enforces, the
                // accumulated header bytes and the declared whole-bundle
                // size, so an over-cap bundle, resident or still on the
                // wire, is refused before a single payload byte drains.
                debug!("Bundle exceeds max_bundle_size: {size} > {max}; refused");
                return Received::Refused;
            }
            Err(HeaderFailure::Invalid { report, .. }) => {
                let reason = match report {
                    Some((bundle, reception, reason)) => {
                        // Complete but invalid, with a recoverable id, so
                        // the drop is reported as the sibling gate and drain
                        // drops are (RFC 9171 Sections 5.6 and 5.10). A
                        // structural failure, or a stream that ended inside
                        // the headers, reports `None`, having no id: the
                        // Section 4.1 discard, outside the reception state
                        // machine.
                        self.report_bundle_reception(
                            &bundle,
                            metadata.received_at(),
                            reception,
                            Some(reason),
                        )
                        .await;
                        reason
                    }
                    None => ReasonCode::BlockUnintelligible,
                };
                count_received_dropped(&reason);
                return Received::Disposed;
            }
        };

        // Reserved primary-block flag bits have no protocol effect, but a
        // peer setting them shows up nowhere else.
        let u = hv.bundle.primary.flags.unrecognised;
        if u != 0 {
            debug!(
                bundle_id = %hv.bundle.primary.id,
                "Bundle primary block has unrecognised flag bits set: {u:#x}"
            );
        }

        // The lifetime and hop checks run before the payload is drained, so
        // a dead bundle is dropped having spooled nothing. The dispatch
        // expiry checkpoint re-checks lifetime after the store, a cheap and
        // harmless overlap.
        if let Some(reason) = hv.gate_reason(metadata.received_at()) {
            count_received_dropped(&reason);
            if let ReasonCode::LifetimeExpired = reason {
                // A bundle that arrives already expired is treated as if it
                // never arrived, rather than amplified into report traffic.
                // The deletion reports of Section 5.10 are for bundles that
                // expire in custody, at the dispatch, forward and deliver
                // expiry checkpoints and in the reaper. Dropping before
                // anything is stored also keeps expired traffic from
                // churning the metadata store's dedup LRU.
                debug!("Bundle arrived already expired; dropped");
                return Received::Disposed;
            }
            self.report_bundle_reception(
                &hv.bundle,
                metadata.received_at(),
                hv.report,
                Some(reason),
            )
            .await;
            return Received::Disposed;
        }

        // The configuration-gated RFC 9171 validity checks run pre-drain
        // alongside the lifetime and hop checks. They are policy rejections
        // beyond structural validity, which a deployment may relax, reported
        // as any other gated drop is (Sections 5.6 and 5.10).
        if let Some(reason) = self.rfc9171_gate_reason(&hv) {
            count_received_dropped(&reason);
            self.report_bundle_reception(
                &hv.bundle,
                metadata.received_at(),
                hv.report,
                Some(reason),
            )
            .await;
            return Received::Disposed;
        }

        // Destructure the verified headers once, here at the gate: the
        // decoded extension fields move into the metadata, where a
        // Classifier may read them, and the begun payload-BIB verifiers, the
        // scheduled block removals and the reception reason are kept for the
        // drain and store below. Nothing downstream needs `hv` whole.
        let HeaderVerify {
            bundle,
            extensions,
            to_remove,
            report,
            deferred_verifiers,
        } = hv;
        metadata.extensions = extensions;

        // The record under construction, built once here at the gate, is
        // the one object that travels through the chain, the route lookup,
        // the drain and the store below, mutated in place. It starts at
        // `Dispatching`, the status it is persisted at, because nothing
        // observes the record before the single insert below: only the
        // finished, classified record ever reaches storage.
        let bundle = Bundle {
            bpv7: bundle,
            metadata,
            status: BundleStatus::Dispatching,
        };

        // Every arrival spools through the store's streaming interface. The
        // resident head may include payload bytes, or the whole bundle, so
        // there is one store path rather than a resident and a streamed
        // fork. The validation belongs here: a `ValidatingReceiver` decorates
        // the arrival and `Store::save_stream` is the validation-blind drain.
        // For an arrival complete at the head the decorator yields the head
        // as the stream's `Final` and settles from it alone, the header pass
        // having already taken the arrival's `Final`, so the inner stream is
        // not pulled again.
        debug_assert!(bundle.metadata.storage_name.is_none());

        // The gate decisions settle before the payload drains, so a
        // rejected arrival returns here having spooled nothing, and the
        // producer learns the verdict from this call's return, never from
        // the stream.
        let Some(Admitted {
            mut bundle,
            decision,
        }) = self
            .decide_at_gate(bundle, headers.clone(), &bcb_ops, report)
            .await
        else {
            return Received::Disposed;
        };

        // The deferred-BIB verifiers were begun by the header pass, in the
        // same keyed scope as the header verify. The decorator absorbs the
        // resident payload prefix at construction and the streamed remainder
        // as it arrives, feeding the payload CRC, the block and outer
        // framing, and each deferred BIB digest as the bytes stream past.
        let payload_start = bundle.bpv7.blocks.get(&1).map_or(headers.len(), |b| {
            usize::try_from(b.payload_range().start)
                .expect("parse bounds pre-payload offsets to usize")
        });
        let mut tail_rx =
            ValidatingReceiver::new(stream, tail, deferred_verifiers, headers, payload_start);

        // Drain the admitted arrival into the store, bounded by
        // `max_bundle_size` as a backstop: the declared size was enforced at
        // parse, and a producer exceeding its declaration trips the framing
        // checks first.
        let max_size = self.max_bundle_size_mem();
        let outcome = self.store.save_stream(&mut tail_rx, max_size).await;

        // Settle the decorator's verdict against the save. A save the
        // verdict then rejects was staged before the verdict settled, and
        // discarding it is this path's half of the streaming contract, the
        // drain being validation-blind.
        let outcome = match outcome {
            Ok((storage_name, len)) => match tail_rx.finish() {
                Ok(()) => Ok((storage_name, len)),
                Err(failure) => {
                    self.store.delete_data(&storage_name).await;
                    Err(failure)
                }
            },
            // A failed drain persisted nothing, and settles through the
            // decorator's verdict. A cancelled drain is the decorator failing
            // a pull, its verdict latched first, a producer that went away
            // latching `Truncated`. The bound cannot trip here: the tail
            // rejects any byte past the outer break before the bound counts
            // it, and the header pass refused a declaration over the same
            // bound.
            Err(ConcatError::Cancelled | ConcatError::TooLarge { .. }) => Err(tail_rx
                .finish()
                .err()
                .unwrap_or(ValidationFailure::Truncated)),
        };

        // Settle the store from the spool's outcome. The bundle is stored
        // exactly as received, nothing being edited on input.
        let (storage_name, data_len) = match outcome {
            Ok(saved) => saved,
            Err(failure) => {
                let Some(reason) = failure.reason_code() else {
                    // Truncated, so the transfer never completed and is
                    // refused, the peer retaining custody and being free to
                    // resend. A refusal is never reported.
                    debug!("Truncated payload; refused");
                    return Received::Refused;
                };
                // Complete but unacceptable: the transfer was accepted, so
                // this node owns the bundle and terminates it, reporting as
                // the sibling gate drops do (RFC 9171 Sections 5.6 and
                // 5.10). Nothing remains staged, the settle above having
                // discarded any save the verdict rejected.
                debug!("Streamed payload rejected: {failure}");
                count_received_dropped(&reason);
                self.report_bundle_reception(
                    &bundle.bpv7,
                    bundle.metadata.received_at(),
                    report,
                    Some(reason),
                )
                .await;
                return Received::Disposed;
            }
        };
        bundle.metadata.storage_name = Some(storage_name.clone());

        // The scheduled block removals travel with the bundle on its
        // metadata, sorted for a deterministic persisted order, and are
        // applied per attempt by the egress rewrite and the deliver strip.
        let mut to_remove: Vec<u64> = to_remove.into_iter().collect();
        to_remove.sort_unstable();
        bundle.metadata.to_remove = to_remove;

        // Only a completely assembled bundle counts as received.
        metrics::counter!("bpa.bundle.received").increment(1);
        metrics::counter!("bpa.bundle.received.bytes").increment(data_len as u64);

        // Reception happened, so report it, where requested, before the
        // duplicate check: RFC 9171 Section 5.6 reports on reception, so a
        // replayed or duplicate bundle is still reported as received. The
        // Ingress chain ran at the pre-drain gate, and a chain drop reported
        // itself there.
        self.report_bundle_reception(&bundle.bpv7, bundle.metadata.received_at(), report, None)
            .await;

        // `insert_metadata` is the authoritative atomic duplicate check.
        // The gate's advisory `seen_recently` probe sees only committed
        // records, so duplicate copies racing in concurrently both spool and
        // settle here, and the loser is dropped and never dispatched.
        //
        // A duplicate bundle that is invalid, and so rejected before reaching
        // here, is not deduplicated: a replay re-parses and may re-report.
        // That is accepted rather than fixed, RFC 9171 status reports being
        // off-by-default debugging aids rather than acknowledgements, so a
        // duplicate is harmless. Tombstone-on-reject suppression is deferred,
        // because the compressed-status-report and custody work inverts the
        // requirement, a resend then meaning "report lost, please
        // re-report", so that design must own the semantics. Ledgered in
        // bpa/docs/TODO.md under "Tombstone-on-reject suppression".
        if !self.store.insert_metadata(&bundle).await {
            // A bundle with a matching id is already in the metadata store.
            metrics::counter!("bpa.bundle.received.duplicate").increment(1);
            // The spool saved this copy's data, and the duplicate loses it.
            self.store.delete_data(&storage_name).await;
            return Received::Disposed;
        }

        // Account the admitted bundle in the status gauge.
        metrics::gauge!("bpa.bundle.status", "state" => status_label(&bundle.status))
            .increment(1.0);

        // Execute the gate's routing decision of record, made again if the
        // table changed during the drain. Fresh arrivals do not transit the
        // dispatch queue, `DispatchPending` belonging to the re-dispatch
        // paths, which are parks, polls, sweeps, restart and transfer
        // outcomes, except the admin-endpoint and fragment arms (see
        // `execute_admitted`). A crash between the insert above and this
        // execution recovers through restart's `Dispatching` arm, which
        // re-dispatches with a fresh lookup.
        self.execute_admitted(bundle, decision).await;
        Received::Dispatched
    }

    // The gate decisions for an admitted header chain: the Ingress filter
    // chain, and then the route lookup, which is the decision of record.
    // They settle before the payload drains, so a rejection, `None`, returns
    // with nothing spooled or persisted. Every rejection is counted and
    // reported here.
    async fn decide_at_gate(
        &self,
        bundle: Bundle,
        headers: Bytes,
        bcb_ops: &HashMap<u64, OperationSet>,
        report: ReceptionReport,
    ) -> Option<Admitted> {
        // The duplicate probe comes before any filter or route work: a
        // replay is DoS-shaped traffic, so a recently committed id settles
        // it for the price of a cache lookup, without touching storage. It
        // is advisory only, copies racing in concurrently, ids past the
        // cache's horizon, and a cold cache after restart all settling at
        // `insert_metadata`'s atomic refusal. Reception is still reported,
        // RFC 9171 Section 5.6 reporting on reception and deduplication
        // being a dispatch concern, exactly as the post-spool duplicate path
        // reports it.
        if self.store.seen_recently(bundle.id()) {
            debug!("Duplicate received bundle detected before the drain");
            metrics::counter!("bpa.bundle.received.duplicate").increment(1);
            self.report_bundle_reception(&bundle.bpv7, bundle.metadata.received_at(), report, None)
                .await;
            return None;
        }

        // The Ingress chain runs pre-drain on the resident prefix: the
        // headers and whatever of the payload has arrived, at least the held
        // peek. It runs synchronously on the record and returns it in every
        // outcome, so a Classifier's metadata deltas survive. A filter
        // reading a payload that is not all resident gets the reader's
        // `NotResident`.
        let bundle = if self.filters.has_ingress() {
            match self
                .filters
                .run_ingress(bundle, headers, bcb_ops, &*self.key_provider)
            {
                ChainOutcome::Continue(bundle, _) => bundle,
                ChainOutcome::Drop(bundle, reason) => {
                    let label = reason.unwrap_or(ReasonCode::NoAdditionalInformation);
                    count_received_dropped(&label);
                    self.report_bundle_reception(
                        &bundle.bpv7,
                        bundle.metadata.received_at(),
                        report,
                        reason,
                    )
                    .await;
                    return None;
                }
            }
        } else {
            bundle
        };

        // The route lookup is the decision of record for this arrival, and
        // carries the table it was made against, so that the commit decides
        // again if the table changed during the drain. An explicit Drop
        // route rejects the bundle here, so doomed traffic is never
        // persisted and the caller's cancel stops its drain mid-stream. The
        // lookup follows the chain so that a filter Drop keeps precedence,
        // and because the Classifier-supplied routing inputs must precede
        // it.
        let decision = self.rib.decide(&bundle);
        if let Some(DispatchAction::Drop(reason)) = decision.action {
            let label = reason.unwrap_or(ReasonCode::NoAdditionalInformation);
            count_received_dropped(&label);
            // A Drop with a reason reports as the sibling gate drops do. A
            // Drop without one is silent, down to the reception report,
            // which a bundle dropped by dispatch's `delete_bundle` path has
            // already had.
            match reason {
                Some(reason) => {
                    debug!("Route lookup drops the received bundle: {reason:?}");
                    self.report_bundle_reception(
                        &bundle.bpv7,
                        bundle.metadata.received_at(),
                        report,
                        Some(reason),
                    )
                    .await;
                }
                None => debug!("Route lookup silently drops the received bundle"),
            }
            return None;
        }

        Some(Admitted { bundle, decision })
    }

    // The configuration-gated RFC 9171 validity checks: policy requirements
    // beyond structural validity, checked pre-drain, everything they read
    // being header material the keyed verify pass has established. CLA
    // ingress and `originate_raw` both apply them.
    pub(super) fn rfc9171_gate_reason(&self, hv: &HeaderVerify) -> Option<ReasonCode> {
        // RFC 9171 Section 4.3.1: "A CRC SHALL be present in the primary
        // block unless the bundle includes a BPSec Block Integrity Block
        // whose target is the primary block". `Maybe` coverage, from an
        // undecryptable BIB, counts as protected, as the block-editing paths
        // assume.
        if self.primary_block_integrity
            && let Some(primary_block) = hv.bundle.blocks.get(&0)
            && matches!(hv.bundle.primary.crc_type, CrcType::None)
            && matches!(primary_block.bib, BibCoverage::None)
        {
            debug!("Rejecting bundle: primary block has no integrity protection (no CRC, no BIB)");
            return Some(ReasonCode::BlockUnintelligible);
        }

        // RFC 9171 Section 4.4.2: "If the bundle's creation time is zero,
        // then the bundle MUST contain exactly one (1) occurrence of [Bundle
        // Age]".
        if self.bundle_age_required
            && !hv.bundle.primary.id.timestamp.is_clocked()
            && hv.extensions.age.is_none()
        {
            debug!("Rejecting bundle: no clock in creation timestamp and no Bundle Age block");
            return Some(ReasonCode::LifetimeExpired);
        }

        None
    }
}
