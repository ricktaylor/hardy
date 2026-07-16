use hardy_bpv7::{parse::PayloadTail, status_report::ReasonCode};

use super::*;
use crate::{bundle::parse, cla::Segment, stream::Receiver};

/// Dumb-spool an oversized payload's tail in memory after the gate has accepted
/// the bundle: feed each remaining segment through [`PayloadTail`] (carrying the
/// payload CRC, the block/outer-break checks, and trailing-data rejection) while
/// accumulating the bytes, then return the assembled bundle. The `BytesMut`
/// accumulator is the single seam streaming storage will later replace.
async fn drain_payload(
    stream: &dyn Receiver<Segment>,
    consumed: Bytes,
    mut tail: PayloadTail,
) -> Option<Bytes> {
    let mut whole = bytes::BytesMut::from(consumed.as_ref());
    loop {
        let (bytes, last) = match stream.recv().await {
            Ok(Segment::Next(b)) => (b, false),
            Ok(Segment::Final(b)) => (b, true),
            Err(_) => {
                debug!("Truncated payload (stream cancelled mid-tail)");
                return None;
            }
        };
        let complete = match tail.push(&bytes) {
            Ok(complete) => complete,
            Err(e) => {
                debug!("Streamed payload rejected: {e}");
                return None;
            }
        };
        whole.extend_from_slice(&bytes);
        if complete {
            break;
        }
        if last {
            debug!("Truncated payload");
            return None;
        }
    }
    Some(whole.freeze())
}

struct CountingReceiver<'a> {
    stream: &'a dyn Receiver<Segment>,
}

#[async_trait]
impl Receiver<Segment> for CountingReceiver<'_> {
    async fn recv(&self) -> Result<Segment, crate::stream::RecvError> {
        let seg = self.stream.recv().await?;
        let len = match &seg {
            Segment::Next(b) | Segment::Final(b) => b.len(),
        };
        metrics::counter!("bpa.bundle.received.bytes").increment(len as u64);
        Ok(seg)
    }
}

impl Dispatcher {
    // Entry point for bundles received from CLAs.
    //
    // Bundle validation errors are handled internally (logged and dropped) rather
    // than returned to the CLA, since the CLA cannot fix invalid bundle content.
    //
    // # Bundle State
    //
    // - Initial status: `New`
    // - Next: `process_received_bundle()` → `ingress_bundle()` → Ingress filter → `Dispatching`
    //
    // See [Bundle State Machine Design](../../docs/bundle_state_machine_design.md)
    // for the complete state transition diagram.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn receive_bundle(
        &self,
        stream: &dyn Receiver<Segment>,
        ingress_cla: Arc<str>,
        ingress_peer_node: Option<&hardy_bpv7::eid::NodeId>,
        ingress_peer_addr: Option<&cla::ClaAddress>,
    ) -> cla::Result<()> {
        metrics::counter!("bpa.bundle.received").increment(1);

        let metadata = bundle::BundleMetadata {
            status: bundle::BundleStatus::New,
            read_only: bundle::ReadOnlyMetadata {
                received_at: time::OffsetDateTime::now_utc(),
                ingress_peer_node: ingress_peer_node.cloned(),
                ingress_peer_addr: ingress_peer_addr.cloned(),
                ingress_cla: Some(ingress_cla),
                ..Default::default()
            },
            ..Default::default()
        };

        let stream = CountingReceiver { stream };
        // Drop sites inside `process_received_bundle` count themselves under
        // `bpa.bundle.received.dropped` with a `reason` label. Nothing was
        // stored on the CLA path before a drop, so there's no data to clean up.
        if let Some((bundle, data)) = self.process_received_bundle(&stream, metadata).await {
            self.ingress_bundle(bundle, data).await;
        }
        Ok(())
    }

    // Shared bundle processing: parse, validate, store, and report.
    //
    // Called from both the CLA ingress path (`receive_bundle`) and the ADU
    // reassembly path (`reassemble`). Handles all bundle validation internally
    // — invalid bundles are logged, counted, and dropped with status reports
    // where possible.
    //
    // Returns `Some((bundle, data))` for valid bundles ready for ingress,
    // or `None` if the bundle was dropped (invalid, duplicate, etc.).
    //
    // If `metadata.storage_name` is already set (reassembly case), the existing
    // stored data is used. Otherwise (CLA case), the data is saved after parsing.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub(super) async fn process_received_bundle(
        &self,
        stream: &dyn Receiver<Segment>,
        mut metadata: bundle::BundleMetadata,
    ) -> Option<(bundle::Bundle, Bytes)> {
        // Pre-drain header pass: parse the header chain off the stream and run
        // keyed header verification — both in `bundle::parse`, before an oversized
        // payload is spooled. `Err` carries an optional reception report to emit
        // before dropping (reporting stays here — we own the machinery); a
        // structural / truncation drop carries no recoverable bundle.
        let (mut hv, headers, tail) = match parse::parse_headers(stream, self.key_provider()).await
        {
            Ok(parts) => parts,
            Err(report) => {
                let reason = match report {
                    Some((bundle, reason)) => {
                        let bundle = bundle::Bundle { metadata, bundle };
                        self.report_bundle_reception(&bundle, reason).await;
                        reason
                    }
                    None => ReasonCode::BlockUnintelligible,
                };
                metrics::counter!("bpa.bundle.received.dropped", "reason" => crate::otel_metrics::reason_label(&reason)).increment(1);
                return None;
            }
        };

        // Early-reject gate (lifetime / hop) before the payload is drained, so a
        // dead bundle is dropped having spooled nothing. (`Bundle::has_expired`
        // re-checks lifetime post-store in the ingress filter — a cheap, harmless
        // overlap.)
        if let Some(reason) = hv.gate_reason(metadata.read_only.received_at) {
            metrics::counter!("bpa.bundle.received.dropped", "reason" => crate::otel_metrics::reason_label(&reason)).increment(1);
            if let ReasonCode::LifetimeExpired = reason {
                // A bundle that arrives already expired is treated as if it
                // never arrived, not amplified into report traffic — §5.10
                // deletion reports are for bundles that expire in custody (the
                // validity filter and reaper paths). Dropping before anything
                // is stored also keeps expired traffic from churning the
                // metadata store's dedup LRU.
                debug!("Bundle arrived already expired; dropped");
                return None;
            }
            metadata.read_only.previous_node = hv.extracted.previous_node;
            metadata.read_only.age = hv.extracted.age;
            metadata.read_only.hop_count = hv.extracted.hop_count;
            let bundle = bundle::Bundle {
                metadata,
                bundle: hv.bundle,
            };
            self.report_bundle_reception(&bundle, ReasonCode::NoAdditionalInformation)
                .await;
            self.report_bundle_deletion(&bundle, reason).await;
            return None;
        }

        // Gate passed — drain the payload (oversized case), then finalize.
        let whole = match tail {
            None => headers,
            Some(tail) => match drain_payload(stream, headers, tail).await {
                Some(whole) => whole,
                None => {
                    metrics::counter!("bpa.bundle.received.dropped", "reason" => crate::otel_metrics::reason_label(&ReasonCode::BlockUnintelligible)).increment(1);
                    return None;
                }
            },
        };

        // Post-drain finalize: verify the deferred block-1 BIB targets and apply
        // §E rewrites. The decoded extension fields were captured at header time
        // and the §E rewrite only removes blocks, so move `hv.extracted` into the
        // metadata now (`take` leaves `hv` intact for finalize, which ignores it).
        metadata.read_only.previous_node = hv.extracted.previous_node.take();
        metadata.read_only.age = hv.extracted.age.take();
        metadata.read_only.hop_count = hv.extracted.hop_count.take();
        let (bundle, chunks, report_reason) = match parse::finalize_with_provider(
            &whole,
            hv,
            self.key_provider(),
        ) {
            Ok(x) => x,
            Err((bundle, error)) => {
                debug!("Invalid bundle received: {error}");
                let reason = parse::status_report_reason_for(&error);
                metrics::counter!("bpa.bundle.received.dropped", "reason" => crate::otel_metrics::reason_label(&reason)).increment(1);
                let bundle = bundle::Bundle { metadata, bundle };
                self.report_bundle_reception(&bundle, reason).await;
                return None;
            }
        };

        // Persist (flatten any rewrite chunks first).
        let data = match chunks {
            None => whole,
            Some(chunks) => hardy_bpv7::editor::Chunk::flatten_bytes(chunks, whole),
        };
        // `Some` here = the caller pre-stored the data (reassembly / restart) and
        // owns its cleanup; on any drop we just return `None` and the caller
        // deletes it. We only delete storage *we* create (the CLA `save_data`
        // path below), and only on the post-store duplicate path.
        let mut caller_stored = false;
        if let Some(storage_name) = &metadata.storage_name {
            self.store.replace_data(storage_name, data.clone()).await;
            caller_stored = true;
        } else {
            metadata.storage_name = Some(self.store.save_data(data.clone()).await);
        }
        let bundle = bundle::Bundle { metadata, bundle };

        // Reception happened, so report it (when requested) before the duplicate
        // check: RFC 9171 §5.6 reports on reception, and dedup belongs to the
        // later dispatch step — so a replayed/duplicate bundle is still reported
        // as received.
        self.report_bundle_reception(&bundle, report_reason).await;

        // `insert_metadata` is the authoritative atomic dup check — the one place
        // a duplicate is caught, so a duplicate *valid* bundle is dropped here and
        // never double-dispatched. We don't pre-check existence earlier: that would
        // add a metadata read to every received bundle to catch a comparatively
        // rare replay.
        //
        // A duplicate *invalid* bundle (rejected before reaching here) isn't
        // deduplicated — a replay re-parses and may re-report. Accepted, not fixed:
        // RFC 9171 status reports are off-by-default debugging aids, not acks, so a
        // duplicate is harmless. Tombstone-on-reject suppression is deferred — the
        // future compressed-status-report / custody work inverts the requirement (a
        // resend then means "report lost, please re-report"), so that design must
        // own the semantics. See review_refactor_parse.md item 4.
        if !self.store.insert_metadata(&bundle).await {
            // Bundle with matching id already exists in the metadata store.
            metrics::counter!("bpa.bundle.received.duplicate").increment(1);
            // Delete the data only if we saved it here (CLA path); a caller that
            // pre-stored deletes its own on the `None` return.
            if !caller_stored && let Some(storage_name) = &bundle.metadata.storage_name {
                self.store.delete_data(storage_name).await;
            }
            return None;
        }

        Some((bundle, data))
    }

    // Run the Ingress filter, checkpoint to `Dispatching`, and route the bundle.
    //
    // # Processing Steps
    //
    // 1. Execute Ingress filter hook
    // 2. Persist any filter mutations (crash-safe ordering)
    // 3. **Checkpoint**: Transition status to `Dispatching`
    // 4. Call `process_bundle()` for routing decision
    //
    // # Crash Safety
    //
    // The checkpoint to `Dispatching` is always persisted after the Ingress
    // filter completes. On restart, bundles in `New` status re-run from this
    // function, while bundles in `Dispatching` skip directly to routing.
    //
    // See [Filter Subsystem Design](../../docs/filter_subsystem_design.md) for
    // filter execution details.
    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.bundle.primary.id)))]
    pub(super) async fn ingress_bundle(&self, bundle: bundle::Bundle, data: Bytes) {
        metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&bundle.metadata.status)).increment(1.0);

        // Ingress filter hook (includes bundle-validity: flags, lifetime, hop-count)
        match self
            .filter_engine
            .exec(filter::Hook::Ingress, bundle, data, self.key_provider())
            .await
            // TODO: Recover gracefully once filter error handling is redesigned
            .trace_expect("Ingress filter execution failed")
        {
            filter::ExecResult::Continue(mutation, mut bundle, data) => {
                if mutation.data
                    && let Some(storage_name) = &bundle.metadata.storage_name
                {
                    self.store.replace_data(storage_name, data.clone()).await;
                }

                // Always checkpoint to Dispatching (crash safety)
                metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&bundle.metadata.status)).decrement(1.0);
                bundle.metadata.status = bundle::BundleStatus::Dispatching;
                metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&bundle.metadata.status)).increment(1.0);
                self.store.update_metadata(&bundle).await;

                // Hand off to dispatch queue for fan-out via processing pool
                self.dispatch_bundle(bundle).await
            }
            filter::ExecResult::Drop(bundle, Some(reason)) => {
                self.drop_bundle(bundle, reason).await
            }
            filter::ExecResult::Drop(bundle, None) => self.delete_bundle(bundle).await,
        }
    }
}
