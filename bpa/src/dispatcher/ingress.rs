use alloc::sync::Arc;

use hardy_bpv7::{
    block::BibCoverage, bpsec::bcb::OperationSet, crc::CrcType, eid::NodeId,
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
        parse::{HeaderFailure, HeaderVerify, ReceptionReport, parse_headers},
    },
    cla::{Acceptance, ClaAddress, Segment},
    filter::ChainOutcome,
    otel_metrics::{reason_label, status_label},
    routing::{Decision, DispatchAction},
    stream::{ConcatError, Receiver},
};

// An arrival the gate decisions (Ingress chain + route lookup) admitted.
// The gate returns `None` for a rejection, which it has already counted and
// reported.
struct Admitted {
    bundle: Bundle,
    // The routing decision of record, with the table it was made against.
    decision: Decision,
}

// The outcome of the shared receive pipeline, for the three in-feeds.
//
// `Dispatched` and `Disposed` are both *acceptance* (the bundle ran the
// Ingress chain and its routing decision was executed, or it was dropped
// internally with reports); `Refused` is the one non-acceptance outcome — the
// transfer could not be taken at all (truncation, the size cap) and its
// custodian keeps responsibility.
pub(super) enum Received {
    /// Admitted: the bundle ran the Ingress chain, was written once to the
    /// metadata store, and had the gate's routing decision executed (or
    /// queued for dispatch).
    Dispatched,
    /// Accepted and disposed of internally (invalid, gate-rejected,
    /// chain-dropped, duplicate) — reports emitted where possible; nothing
    /// dispatched.
    Disposed,
    /// Acceptance refused: truncated stream or over the size cap. The
    /// refusal site logs the specifics.
    Refused,
}

impl Dispatcher {
    // Entry point for bundles received from CLAs.
    //
    // The return value is the acceptance verdict (the `cla::Sink::dispatch`
    // contract): `Accepted` covers every internally-disposed case (invalid,
    // gate-rejected, duplicate) too — refusing an invalid bundle would
    // invite the previous node's custody/retransmission machinery to resend
    // identical bytes forever, so the first BPA to detect a content problem
    // accepts and terminates the bundle. `Refused` is reserved for transfers
    // this node could not take at all (truncation, the size cap). Returning
    // without draining `stream` is deliberately meaningless on its own: the
    // producer learns the verdict from this return, never from the stream
    // closing.
    //
    // # Bundle State
    //
    // - `process_received_bundle()` runs the Ingress filter and the gate
    //   route lookup, writes the record once at `Dispatching`, and executes
    //   the routing decision through `execute_admitted` — fresh arrivals do
    //   not transit the dispatch queue (`DispatchPending` belongs to the
    //   re-dispatch paths), except the admin-endpoint and fragment arms,
    //   which queue for the processing pool.
    //
    // See [Bundle State Machine Design](../../docs/bundle_state_machine_design.md)
    // for the complete state transition diagram.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn receive_bundle(
        &self,
        ingress_cla: Arc<str>,
        ingress_peer_node: Option<&NodeId>,
        ingress_peer_addr: Option<&ClaAddress>,
        stream: &mut dyn Receiver<Segment>,
    ) -> Acceptance {
        let metadata = BundleMetadata::ingress(
            ingress_cla,
            ingress_peer_node.cloned(),
            ingress_peer_addr.cloned(),
        );

        // Truncation and the size bound refuse the transfer — the CLA must
        // not acknowledge it, so the peer retransmits — while
        // invalid-but-complete bundles are accepted and handled internally:
        // the CLA cannot fix invalid content, and the transfer itself
        // succeeded.
        //
        // Drop sites inside `process_received_bundle` count themselves under
        // `bpa.bundle.received.dropped` with a `reason` label. Nothing was
        // stored on the CLA path before a drop, so there's no data to clean up.
        match self.process_received_bundle(stream, metadata).await {
            Received::Dispatched | Received::Disposed => Acceptance::Accepted,
            Received::Refused => Acceptance::Refused,
        }
    }

    // Shared bundle processing: parse and validate the headers, gate them
    // (the Ingress chain and the route lookup among the gate's checks), drain
    // and store the payload, report, and execute the routing decision.
    //
    // Called from the CLA ingress path (`receive_bundle`), the ADU
    // reassembly path (`reassemble`), and restart orphan recovery. Handles
    // all bundle validation internally — invalid bundles are logged,
    // counted, and dropped with status reports where possible (`Disposed`);
    // only truncation and the size cap refuse (`Refused`). An admitted
    // bundle runs the Ingress chain, is written once to the metadata store
    // (the P1 checkpoint), and has its gate routing decision executed
    // (`Dispatched`).
    //
    // Every caller hands the bundle as a segment stream — a caller holding
    // whole bytes passes them directly (`Bytes` is a `Receiver<Segment>`) —
    // and the spool saves an admitted bundle fresh: `metadata.storage_name`
    // must be unset, and a caller replaying pre-stored bytes (reassembly,
    // restart orphans) owns its own copy's cleanup after this returns.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub(super) async fn process_received_bundle(
        &self,
        stream: &mut dyn Receiver<Segment>,
        mut metadata: BundleMetadata,
    ) -> Received {
        // Pre-drain header pass: parse the header chain off the stream and run
        // keyed header verification — both in `bundle::parse`, before a payload
        // still arriving is spooled. `Err` carries an optional recoverable bundle to
        // report before dropping (reporting stays here — we own the machinery);
        // a structural failure, or a stream that ended inside the headers,
        // carries none. The pass holds the Ingress chain's payload peek: none
        // for an empty chain.
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
                // Covers both bounds `parse_headers` enforces: the
                // accumulated header bytes and the declared whole-bundle
                // size — an over-cap bundle, resident or still on the
                // wire, refuses before a single payload byte drains.
                debug!("Bundle exceeds max_bundle_size: {size} > {max}; refused");
                return Received::Refused;
            }
            Err(HeaderFailure::Invalid { report, .. }) => {
                let reason = match report {
                    Some((bundle, reception, reason)) => {
                        // Complete but invalid, with a recoverable id: the
                        // drop is reported like the sibling gate and drain
                        // drops (RFC 9171 §5.6/§5.10). A structural failure,
                        // or a stream that ended inside the headers (`None`),
                        // has no id to report: the §4.1 discard, outside the
                        // reception state machine.
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
                metrics::counter!("bpa.bundle.received.dropped", "reason" => reason_label(&reason))
                    .increment(1);
                return Received::Disposed;
            }
        };

        // Reserved primary-block flag bits have no protocol effect, but they are
        // the only sign of a peer setting them: say so.
        let u = hv.bundle.primary.flags.unrecognised;
        if u != 0 {
            debug!(
                bundle_id = %hv.bundle.primary.id,
                "Bundle primary block has unrecognised flag bits set: {u:#x}"
            );
        }

        // Early-reject gate (lifetime / hop) before the payload is drained, so a
        // dead bundle is dropped having spooled nothing. (`Bundle::has_expired`
        // re-checks lifetime post-store in the ingress filter — a cheap, harmless
        // overlap.)
        if let Some(reason) = hv.gate_reason(metadata.received_at()) {
            metrics::counter!("bpa.bundle.received.dropped", "reason" => reason_label(&reason))
                .increment(1);
            if let ReasonCode::LifetimeExpired = reason {
                // A bundle that arrives already expired is treated as if it
                // never arrived, not amplified into report traffic — §5.10
                // deletion reports are for bundles that expire in custody (the
                // dispatch, forward, and deliver expiry checkpoints and the
                // reaper). Dropping before anything
                // is stored also keeps expired traffic from churning the
                // metadata store's dedup LRU.
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

        // Config-gated RFC 9171 validity checks, at the same pre-drain seat
        // as the lifetime/hop gate: policy rejections that go beyond
        // structural validity (deployments may relax them), reported like
        // any other gated drop (§5.6/§5.10).
        if let Some(reason) = self.rfc9171_gate_reason(&hv) {
            metrics::counter!("bpa.bundle.received.dropped", "reason" => reason_label(&reason))
                .increment(1);
            self.report_bundle_reception(
                &hv.bundle,
                metadata.received_at(),
                hv.report,
                Some(reason),
            )
            .await;
            return Received::Disposed;
        }

        // Destructure the verified headers once, here at the gate: move the
        // decoded extension fields into metadata (a Classifier may read them),
        // and keep the begun payload-BIB verifiers, the scheduled §E removals,
        // and the reception reason for the drain and store below. Nothing
        // downstream needs `hv` whole.
        let HeaderVerify {
            bundle,
            extensions,
            to_remove,
            report,
            deferred_verifiers,
        } = hv;
        metadata.extensions = extensions;

        // The record under construction: built once here at the gate, it is
        // the one object that travels through the chain, the route lookup,
        // the drain, and the store below, mutated in place. It is born at
        // `Dispatching` — the status it is persisted at — because nothing
        // observes the record before the single insert below: only the
        // finished, classified record ever reaches storage.
        let bundle = Bundle {
            bpv7: bundle,
            metadata,
            status: BundleStatus::Dispatching,
        };

        // Every arrival spools through the store's streaming seam — the
        // resident head may include payload bytes, or the whole bundle, so
        // there is one store path, not a resident/streamed fork. The door
        // owns the validation: a `ValidatingReceiver` decorates the arrival
        // and `Store::save_stream` is the validation-blind drain. For a
        // complete-at-head arrival the decorator yields the head as the
        // stream's `Final` and settles from it alone: the header pass already
        // took the arrival's `Final`, so the inner stream is not pulled again.
        debug_assert!(bundle.metadata.storage_name.is_none());

        // The gate decisions settle before the payload drains, so a rejected
        // arrival returns here having spooled nothing; the producer learns
        // the verdict from the door's return, never from the stream.
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
        // same keyed scope as the header verify; the decorator absorbs the
        // resident payload prefix at construction, the streamed remainder
        // as it arrives — feeding the payload CRC, the block+outer framing,
        // and each deferred BIB digest as the bytes stream past.
        let payload_start = bundle
            .bpv7
            .blocks
            .get(&1)
            .map_or(headers.len(), |b| b.payload_range().start as usize);
        let mut tail_rx =
            ValidatingReceiver::new(stream, tail, deferred_verifiers, headers, payload_start);

        // Drain the admitted arrival into the store, bounded by
        // `max_bundle_size` as the defensive backstop (the declared size was
        // already enforced at parse; a producer exceeding its declaration
        // trips the framing checks first).
        let max_size = self.max_bundle_size_mem();
        let outcome = self.store.save_stream(&mut tail_rx, max_size).await;

        // Settle the decorator's verdict against the save. A save the
        // verdict then rejects was staged before the verdict settled — the
        // discard half of the streaming contract, owed by this door now
        // that the drain is validation-blind.
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
            // a pull, its verdict latched first (a producer that went away
            // latches `Truncated`). The defensive bound cannot trip: the tail
            // rejects any byte past the outer break before the bound counts
            // it, and the header pass refused a declaration over the same
            // bound.
            Err(ConcatError::Cancelled | ConcatError::TooLarge { .. }) => Err(tail_rx
                .finish()
                .err()
                .unwrap_or(ValidationFailure::Truncated)),
        };

        // Settle the store from the spool's outcome. The bundle is stored
        // exactly as received — no editing on input.
        let (storage_name, data_len) = match outcome {
            Ok(saved) => saved,
            Err(failure) => {
                let Some(reason) = failure.reason_code() else {
                    // Truncated: the transfer never completed, so it is
                    // refused — the peer retains custody and may resend.
                    // A refusal is never reported.
                    debug!("Truncated payload; refused");
                    return Received::Refused;
                };
                // Complete but unacceptable: the transfer was accepted,
                // so this node owns the bundle and terminates it —
                // reported like the sibling gate drops (RFC 9171
                // §5.6/§5.10). Nothing remains staged: the settle above
                // discarded any save the verdict rejected.
                debug!("Streamed payload rejected: {failure}");
                metrics::counter!("bpa.bundle.received.dropped", "reason" => reason_label(&reason))
                    .increment(1);
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

        // The §E removals travel with the bundle to the output doors, sorted
        // for a deterministic persisted order; they ride the metadata and are
        // applied per-attempt (egress rewrite, deliver strip).
        let mut to_remove: Vec<u64> = to_remove.into_iter().collect();
        to_remove.sort_unstable();
        bundle.metadata.to_remove = to_remove;

        // Only a completely assembled bundle counts as received.
        metrics::counter!("bpa.bundle.received").increment(1);
        metrics::counter!("bpa.bundle.received.bytes").increment(data_len as u64);

        // Reception happened, so report it (when requested) before the duplicate
        // check: RFC 9171 §5.6 reports on reception, so a replayed/duplicate
        // bundle is still reported as received. (The Ingress chain already ran
        // at the pre-drain gate; a chain drop reported itself there.)
        self.report_bundle_reception(&bundle.bpv7, bundle.metadata.received_at(), report, None)
            .await;

        // `insert_metadata` is the authoritative atomic dup check — the one place
        // a duplicate is caught, so a duplicate *valid* bundle is dropped here and
        // never dispatched. We don't pre-check existence earlier: that would add a
        // metadata read to every received bundle to catch a comparatively rare
        // replay.
        //
        // A duplicate *invalid* bundle (rejected before reaching here) isn't
        // deduplicated — a replay re-parses and may re-report. Accepted, not fixed:
        // RFC 9171 status reports are off-by-default debugging aids, not acks, so a
        // duplicate is harmless. Tombstone-on-reject suppression is deferred — the
        // future compressed-status-report / custody work inverts the requirement (a
        // resend then means "report lost, please re-report"), so that design must
        // own the semantics. Ledgered in bpa/docs/TODO.md ("Tombstone-on-reject
        // suppression").
        if !self.store.insert_metadata(&bundle).await {
            // Bundle with matching id already exists in the metadata store.
            metrics::counter!("bpa.bundle.received.duplicate").increment(1);
            // The spool saved this copy's data; the duplicate loses it.
            self.store.delete_data(&storage_name).await;
            return Received::Disposed;
        }

        // Account the admitted bundle in the status gauge.
        metrics::gauge!("bpa.bundle.status", "state" => status_label(&bundle.status))
            .increment(1.0);

        // Execute the gate's routing decision of record, made again if the
        // table changed during the drain. Fresh arrivals do not transit the
        // dispatch queue (`DispatchPending` belongs to the re-dispatch paths:
        // parks, polls, sweeps, restart, transfer outcomes), except the
        // admin-endpoint and fragment arms (see `execute_admitted`). A crash
        // between the insert above and this execution recovers through
        // restart's `Dispatching` arm, which re-dispatches with a fresh
        // lookup.
        self.execute_admitted(bundle, decision).await;
        Received::Dispatched
    }

    // The gate decisions for an admitted header chain: the Ingress filter
    // chain, then the route lookup — the decision of record. These settle
    // before the payload drains, so a rejection (`None`) returns with
    // nothing spooled or persisted. All rejection counting and reporting
    // happens here.
    async fn decide_at_gate(
        &self,
        bundle: Bundle,
        headers: Bytes,
        bcb_ops: &HashMap<u64, OperationSet>,
        report: ReceptionReport,
    ) -> Option<Admitted> {
        // Ingress chain at the pre-drain gate, on the resident prefix: the
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
                Ok(ChainOutcome::Continue(bundle, _)) => bundle,
                Ok(ChainOutcome::Drop(bundle, reason)) => {
                    let label = reason.unwrap_or(ReasonCode::NoAdditionalInformation);
                    metrics::counter!("bpa.bundle.received.dropped", "reason" => reason_label(&label)).increment(1);
                    self.report_bundle_reception(
                        &bundle.bpv7,
                        bundle.metadata.received_at(),
                        report,
                        reason,
                    )
                    .await;
                    return None;
                }
                Err((bundle, e)) => {
                    // The resident prefix failed the chain's own decode pass —
                    // an internal inconsistency, since it parsed at reception.
                    error!("Ingress filter chain failed: {e}");
                    metrics::counter!("bpa.bundle.received.dropped", "reason" => reason_label(&ReasonCode::BlockUnintelligible)).increment(1);
                    self.report_bundle_reception(
                        &bundle.bpv7,
                        bundle.metadata.received_at(),
                        report,
                        Some(ReasonCode::BlockUnintelligible),
                    )
                    .await;
                    return None;
                }
            }
        } else {
            bundle
        };

        // Route at the gate — the decision of record for this arrival,
        // carrying the table it was made against: the commit decides again
        // if the table changed during the drain. An explicit Drop route
        // rejects the bundle here — doomed traffic is never persisted, and
        // the caller's cancel stops its drain mid-stream. Placement after
        // the chain is deliberate: a filter Drop keeps precedence, and the
        // Classifier-supplied routing inputs must precede the lookup.
        let decision = self.rib.decide(&bundle);
        if let Some(DispatchAction::Drop(reason)) = decision.action {
            let label = reason.unwrap_or(ReasonCode::NoAdditionalInformation);
            metrics::counter!("bpa.bundle.received.dropped", "reason" => reason_label(&label))
                .increment(1);
            // Drop-with-reason reports like the sibling gate drops;
            // Drop-without-reason is silent — not even the reception
            // report, which a bundle dropped by dispatch's delete_bundle
            // path has already had.
            match reason {
                Some(reason) => {
                    debug!("Route lookup drops the bundle at the ingress gate: {reason:?}");
                    self.report_bundle_reception(
                        &bundle.bpv7,
                        bundle.metadata.received_at(),
                        report,
                        Some(reason),
                    )
                    .await;
                }
                None => debug!("Route lookup silently drops the bundle at the ingress gate"),
            }
            return None;
        }

        Some(Admitted { bundle, decision })
    }

    // The config-gated RFC 9171 validity checks: policy requirements beyond
    // structural validity, checked pre-drain — everything they read is
    // header material the keyed verify pass has already established. Both
    // input doors apply them (the originate raw door at its own gate).
    pub(super) fn rfc9171_gate_reason(&self, hv: &HeaderVerify) -> Option<ReasonCode> {
        // RFC 9171 §4.3.1: "A CRC SHALL be present in the primary block
        // unless the bundle includes a BPSec Block Integrity Block whose
        // target is the primary block". `Maybe` coverage (undecryptable
        // BIBs) counts as protected, as the block-editing paths assume.
        if self.primary_block_integrity
            && let Some(primary_block) = hv.bundle.blocks.get(&0)
            && matches!(hv.bundle.primary.crc_type, CrcType::None)
            && matches!(primary_block.bib, BibCoverage::None)
        {
            debug!("Rejecting bundle: primary block has no integrity protection (no CRC, no BIB)");
            return Some(ReasonCode::BlockUnintelligible);
        }

        // RFC 9171 §4.4.2: "If the bundle's creation time is zero, then the
        // bundle MUST contain exactly one (1) occurrence of [Bundle Age]".
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
