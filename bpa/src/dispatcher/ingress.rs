use hardy_bpv7::{block::BibCoverage, crc::CrcType, status_report::ReasonCode};

use super::*;
use crate::{
    bundle::{parse, tail},
    cla::Segment,
    stream::Receiver,
};

// The outcome of the shared receive pipeline, for the three in-feeds.
//
// `Dispatched` and `Disposed` are both *acceptance* (the bundle ran the
// Ingress chain and was handed to the dispatch queue, or was dropped
// internally with reports); `Refused` is the one non-acceptance outcome — the
// transfer could not be taken at all (truncation, the size cap) and its
// custodian keeps responsibility.
pub(super) enum Received {
    /// Admitted: the bundle ran the Ingress chain, was written once to the
    /// metadata store, and was handed to the dispatch queue.
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
    // - Initial status: `New`
    // - Next: `process_received_bundle()` runs the Ingress filter, writes the
    //   record once at `Dispatching`, and hands it to the dispatch queue
    //   (whose send swaps it to `DispatchPending`).
    //
    // See [Bundle State Machine Design](../../docs/bundle_state_machine_design.md)
    // for the complete state transition diagram.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn receive_bundle(
        &self,
        ingress_cla: Arc<str>,
        ingress_peer_node: Option<&hardy_bpv7::eid::NodeId>,
        ingress_peer_addr: Option<&cla::ClaAddress>,
        stream: &mut dyn Receiver<Segment>,
    ) -> cla::Acceptance {
        let metadata = bundle::BundleMetadata::ingress(
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
            Received::Dispatched | Received::Disposed => cla::Acceptance::Accepted,
            Received::Refused => cla::Acceptance::Refused,
        }
    }

    // Shared bundle processing: parse, validate, store, report, run the
    // Ingress chain, and hand off to the dispatch queue.
    //
    // Called from the CLA ingress path (`receive_bundle`), the ADU
    // reassembly path (`reassemble`), and restart orphan recovery. Handles
    // all bundle validation internally — invalid bundles are logged,
    // counted, and dropped with status reports where possible (`Disposed`);
    // only truncation and the size cap refuse (`Refused`). An admitted
    // bundle runs the Ingress chain, is written once to the metadata store
    // (the P1 checkpoint), and is queued for dispatch (`Dispatched`).
    //
    // If `metadata.storage_name` is already set (reassembly/restart case),
    // the existing stored data is used. Otherwise (CLA case), the data is
    // saved after parsing.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub(super) async fn process_received_bundle(
        &self,
        stream: &mut dyn Receiver<Segment>,
        mut metadata: bundle::BundleMetadata,
    ) -> Received {
        // Pre-drain header pass: parse the header chain off the stream and run
        // keyed header verification — both in `bundle::parse`, before an oversized
        // payload is spooled. `Err` carries an optional recoverable bundle to
        // report before dropping (reporting stays here — we own the machinery);
        // a structural / truncation drop carries no recoverable bundle.
        // The cap as an in-memory bound: on a 32-bit target a cap beyond the
        // address space saturates — nothing larger could be buffered anyway.
        let max_size = usize::try_from(self.max_bundle_size.get()).unwrap_or(usize::MAX);
        let (hv, headers, tail, bcb_ops) = match parse::parse_headers(
            stream,
            max_size,
            self.key_provider(),
        )
        .await
        {
            Ok(parts) => parts,
            Err(parse::HeaderFailure::Cancelled) => {
                debug!("Bundle stream cancelled mid-header; refused");
                return Received::Refused;
            }
            Err(parse::HeaderFailure::TooLarge { size, max }) => {
                debug!("Streamed bundle exceeds max_bundle_size: {size} > {max}; refused");
                return Received::Refused;
            }
            Err(parse::HeaderFailure::Invalid(report)) => {
                let reason = match report {
                    Some((bundle, reason)) => {
                        // Complete but invalid, with a recoverable id: the
                        // drop is reported like the sibling gate and drain
                        // drops (RFC 9171 §5.6/§5.10). A structural failure
                        // (`None`) has no id to report: the §4.1 discard,
                        // outside the reception state machine.
                        self.report_bundle_reception(
                            &bundle,
                            metadata.received_at(),
                            ReasonCode::NoAdditionalInformation,
                            Some(reason),
                        )
                        .await;
                        reason
                    }
                    None => ReasonCode::BlockUnintelligible,
                };
                metrics::counter!("bpa.bundle.received.dropped", "reason" => crate::otel_metrics::reason_label(&reason)).increment(1);
                return Received::Disposed;
            }
        };

        // Early-reject gate (lifetime / hop) before the payload is drained, so a
        // dead bundle is dropped having spooled nothing. (`Bundle::has_expired`
        // re-checks lifetime post-store in the ingress filter — a cheap, harmless
        // overlap.)
        if let Some(reason) = hv.gate_reason(metadata.received_at()) {
            metrics::counter!("bpa.bundle.received.dropped", "reason" => crate::otel_metrics::reason_label(&reason)).increment(1);
            if let ReasonCode::LifetimeExpired = reason {
                // A bundle that arrives already expired is treated as if it
                // never arrived, not amplified into report traffic — §5.10
                // deletion reports are for bundles that expire in custody (the
                // validity filter and reaper paths). Dropping before anything
                // is stored also keeps expired traffic from churning the
                // metadata store's dedup LRU.
                debug!("Bundle arrived already expired; dropped");
                return Received::Disposed;
            }
            self.report_bundle_reception(
                &hv.bundle,
                metadata.received_at(),
                hv.report_reason,
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
            metrics::counter!("bpa.bundle.received.dropped", "reason" => crate::otel_metrics::reason_label(&reason)).increment(1);
            self.report_bundle_reception(
                &hv.bundle,
                metadata.received_at(),
                hv.report_reason,
                Some(reason),
            )
            .await;
            return Received::Disposed;
        }

        // Destructure the verified headers once, here at the gate: move the
        // decoded extension fields into metadata (a Classifier may read them),
        // and keep the wire bundle, the begun payload-BIB verifiers, the
        // scheduled §E removals, and the reception reason for the drain and
        // store below. Nothing downstream needs `hv` whole.
        let parse::HeaderVerify {
            bundle,
            extracted,
            to_remove,
            report_reason,
            deferred_verifiers,
        } = hv;
        metadata.extensions = extracted;

        // Ingress chain at the pre-drain gate, on the resident header prefix.
        // It runs synchronously on a throwaway record: the wire bundle is
        // cloned so the original stays available for the drain and the stored
        // record, while the real metadata moves through so a Classifier's
        // deltas survive. A chain drop here is
        // pre-store — nothing was spooled — and is reported like the sibling
        // gates above. A filter reading the
        // not-yet-resident payload gets the reader's not-resident `None`. The
        // clone and this whole block dissolve in the streaming leg, where the
        // chain reads the live prefix directly.
        let (mut metadata, headers) = if self.filters.has_ingress() {
            let record = bundle::Bundle {
                metadata,
                bundle: bundle.clone(),
                status: bundle::BundleStatus::New,
            };
            match self
                .filters
                .run_ingress(record, headers, &bcb_ops, &*self.key_provider)
            {
                Ok(filter::ChainOutcome::Continue(record, prefix)) => (record.metadata, prefix),
                Ok(filter::ChainOutcome::Drop(record, reason)) => {
                    let label = reason.unwrap_or(ReasonCode::NoAdditionalInformation);
                    metrics::counter!("bpa.bundle.received.dropped", "reason" => crate::otel_metrics::reason_label(&label)).increment(1);
                    self.report_bundle_reception(
                        &record.bundle,
                        record.metadata.received_at(),
                        report_reason,
                        reason,
                    )
                    .await;
                    return Received::Disposed;
                }
                Err((record, e)) => {
                    // The resident prefix failed the chain's own decode pass —
                    // an internal inconsistency, since it parsed at reception.
                    error!("Ingress filter chain failed: {e}");
                    metrics::counter!("bpa.bundle.received.dropped", "reason" => crate::otel_metrics::reason_label(&ReasonCode::BlockUnintelligible)).increment(1);
                    self.report_bundle_reception(
                        &record.bundle,
                        record.metadata.received_at(),
                        report_reason,
                        Some(ReasonCode::BlockUnintelligible),
                    )
                    .await;
                    return Received::Disposed;
                }
            }
        } else {
            (metadata, headers)
        };

        // Drain the payload tail (oversized case) through the validating
        // TailReceiver: it feeds the payload CRC / block+outer breaks and each
        // deferred BIB digest as the bytes stream past, accumulating the whole
        // bundle bounded by `max_size` (the amplification guard — the declared
        // length is not trusted). A resident bundle (`tail` None) was already
        // fully validated by the header pass: the payload was present, so no
        // BIB was deferred and its CRC was checked inline.
        let whole = match tail {
            None => headers,
            Some(tail) => {
                // The deferred-BIB verifiers were begun by the header pass, in
                // the same keyed scope as the header verify; the resident
                // payload prefix is absorbed by `TailReceiver::new`, the
                // streamed remainder as it arrives.
                let payload_start = bundle
                    .blocks
                    .get(&1)
                    .map_or(headers.len(), |b| b.payload_range().start as usize);
                let mut tail_rx = tail::TailReceiver::new(
                    stream,
                    tail,
                    deferred_verifiers,
                    &headers.slice(payload_start..),
                );

                // Accumulate onto the resident prefix, reusing its allocation
                // when unshared (a CLA still holding the `Bytes` forces a copy).
                let mut whole = headers
                    .try_into_mut()
                    .unwrap_or_else(|b| crate::BytesMut::from(b.as_ref()));
                loop {
                    let (bytes, last) = match tail_rx.recv().await {
                        Ok(Segment::Next(b)) => (b, false),
                        Ok(Segment::Final(b)) => (b, true),
                        // A failing pull records the reason in `tail_rx`;
                        // `finish` categorises it below.
                        Err(_) => break,
                    };
                    if whole.len().saturating_add(bytes.len()) > max_size {
                        debug!("Streamed bundle exceeds max_bundle_size; refused");
                        return Received::Refused;
                    }
                    whole.extend_from_slice(&bytes);
                    if last {
                        break;
                    }
                }

                match tail_rx.finish() {
                    Ok(()) => whole.freeze(),
                    Err(failure) => {
                        let Some(reason) = failure.reason_code() else {
                            // Truncated: the transfer never completed, so it
                            // is refused — the peer retains custody and may
                            // resend. A refusal is never reported.
                            debug!("Truncated payload; refused");
                            return Received::Refused;
                        };
                        // Complete but unacceptable: the transfer was
                        // accepted, so this node owns the bundle and
                        // terminates it — reported like the sibling gate
                        // drops (RFC 9171 §5.6/§5.10). Nothing was stored:
                        // the failure precedes the save below.
                        debug!("Streamed payload rejected: {failure}");
                        metrics::counter!("bpa.bundle.received.dropped", "reason" => crate::otel_metrics::reason_label(&reason)).increment(1);
                        self.report_bundle_reception(
                            &bundle,
                            metadata.received_at(),
                            report_reason,
                            Some(reason),
                        )
                        .await;
                        return Received::Disposed;
                    }
                }
            }
        };

        // The §E removals travel with the bundle to the output doors, sorted
        // for a deterministic persisted order.
        let mut to_remove: Vec<u64> = to_remove.into_iter().collect();
        to_remove.sort_unstable();

        // The bundle is stored exactly as received — no editing on input.
        // The §E removals ride the metadata and are applied per-attempt at
        // the output doors (egress rewrite, deliver strip).
        let data = whole;
        metadata.to_remove = to_remove;
        // The caller pre-stored the data (reassembly / restart) and owns its
        // cleanup; on any non-dispatched outcome the caller deletes it. We only
        // delete storage *we* create (the CLA `save_data` path below), on the
        // duplicate path.
        let mut caller_stored = false;
        if let Some(storage_name) = &metadata.storage_name {
            self.store.replace_data(storage_name, data.clone()).await;
            caller_stored = true;
        } else {
            metadata.storage_name = Some(self.store.save_data(data.clone()).await);
        }
        let mut bundle = bundle::Bundle {
            bpv7: bundle,
            metadata,
            status: bundle::BundleStatus::New,
        };

        // Only a completely assembled bundle counts as received.
        metrics::counter!("bpa.bundle.received").increment(1);
        metrics::counter!("bpa.bundle.received.bytes").increment(data.len() as u64);

        // Reception happened, so report it (when requested) before the duplicate
        // check: RFC 9171 §5.6 reports on reception, so a replayed/duplicate
        // bundle is still reported as received. (The Ingress chain already ran
        // at the pre-drain gate; a chain drop reported itself there.)
        self.report_bundle_reception(
            &bundle.bundle,
            bundle.metadata.received_at(),
            report_reason,
            None,
        )
        .await;

        // Promote to the queued checkpoint before the single write. `New` is a
        // purely in-memory "under construction" marker: the chain ran at the
        // gate above, and only the finished, classified record is ever
        // persisted — directly at `Dispatching`, the one metadata write per
        // received bundle. The dispatch send's conditional swap to
        // `DispatchPending` is the queue commit, and a crash between the two
        // recovers via the `Dispatching` restart arm. No chain-incomplete
        // record ever reaches storage.
        bundle.status = bundle::BundleStatus::Dispatching;

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
            // Delete the data only if we saved it here (CLA path); a caller
            // that pre-stored deletes its own on the `Disposed` return.
            if !caller_stored && let Some(storage_name) = &bundle.metadata.storage_name {
                self.store.delete_data(storage_name).await;
            }
            return Received::Disposed;
        }

        // Account the admitted bundle in the status gauge; the dispatch send's
        // swap moves it on to `DispatchPending`.
        metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&bundle.status)).increment(1.0);

        self.dispatch_bundle(bundle).await;
        Received::Dispatched
    }

    // The config-gated RFC 9171 validity checks: policy requirements beyond
    // structural validity, checked pre-drain — everything they read is
    // header material the keyed verify pass has already established.
    fn rfc9171_gate_reason(&self, hv: &parse::HeaderVerify) -> Option<ReasonCode> {
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
            && hv.extracted.age.is_none()
        {
            debug!("Rejecting bundle: no clock in creation timestamp and no Bundle Age block");
            return Some(ReasonCode::LifetimeExpired);
        }

        None
    }
}
