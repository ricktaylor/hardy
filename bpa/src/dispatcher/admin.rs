use hardy_bpv7::status_report::AdministrativeRecord;

use super::*;
use crate::stream::concat_stream;

impl Dispatcher {
    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.id())))]
    pub(super) async fn administrative_bundle(&self, bundle: bundle::Bundle) {
        metrics::counter!("bpa.admin_record.received").increment(1);

        // This is a bundle for an Admin Endpoint
        if !bundle.primary().flags.is_admin_record {
            debug!(
                "Received a bundle for an administrative endpoint that isn't marked as an administrative record"
            );
            metrics::counter!("bpa.admin_record.unknown").increment(1);
            return self
                .drop_bundle(bundle, ReasonCode::BlockUnintelligible)
                .await;
        }

        let Some((mut bundle, mut stream)) = self.load_or_drop(bundle).await else {
            return;
        };
        // INTERIM BUFFERING: an administrative record is parsed whole. The
        // store holds only bundles bounded and validated at a door, and its
        // load never fails, so the gather neither caps nor errs.
        let data = concat_stream(&mut *stream, usize::MAX)
            .await
            .trace_expect("A stored bundle's load ends in its Final");

        // The record as stored. Its security operations were accepted at
        // the door it entered by, where this node decrypted the payload
        // into the store or refused the bundle (RFC 9172 §1.4), so one
        // still under a BCB here is a record no door of this build wrote:
        // dropped, as the gate drops a bundle its destination cannot
        // accept. The record's block index and its bytes were validated at
        // ingress, so a disagreement between them is a BPA bug or storage
        // corruption, and fatal.
        let block = bundle
            .bpv7
            .blocks
            .get(&1)
            .trace_expect("A stored bundle has no payload block");
        if block.bcb.is_some() {
            debug!("Administrative record still encrypted; dropped");
            return self.drop_bundle(bundle, ReasonCode::BlockUnsupported).await;
        }
        let data = data.slice_ref(
            block
                .payload(&data)
                .trace_expect("A stored bundle's payload extent exceeds its bytes"),
        );

        // The administrative record is the whole payload — reject any trailing
        // bytes after it (a smuggling vector) with `parse_exact`, not `parse`.
        match hardy_cbor::decode::parse_exact(data.as_ref()) {
            Err(e) => {
                debug!("Failed to parse administrative record: {e}");
                metrics::counter!("bpa.admin_record.unknown").increment(1);
                self.drop_bundle(bundle, ReasonCode::BlockUnintelligible)
                    .await
            }
            Ok(AdministrativeRecord::BundleStatusReport(report)) => {
                debug!("Received administrative record: {report:?}");

                // Count each assertion type present in the report
                if report.received.is_some() {
                    metrics::counter!("bpa.status_report.received", "type" => "reception")
                        .increment(1);
                }
                if report.forwarded.is_some() {
                    metrics::counter!("bpa.status_report.received", "type" => "forwarding")
                        .increment(1);
                }
                if report.delivered.is_some() {
                    metrics::counter!("bpa.status_report.received", "type" => "delivery")
                        .increment(1);
                }
                if report.deleted.is_some() {
                    metrics::counter!("bpa.status_report.received", "type" => "deletion")
                        .increment(1);
                }

                // Find a live service to notify
                if let Some(service) = self.rib.find_service(&report.bundle_id.source) {
                    if let Some(assertion) = report.received {
                        service
                            .on_status_notify(
                                &report.bundle_id,
                                &bundle.id().source,
                                services::StatusNotify::Received,
                                report.reason,
                                assertion.0,
                            )
                            .await;
                    }
                    if let Some(assertion) = report.forwarded {
                        service
                            .on_status_notify(
                                &report.bundle_id,
                                &bundle.id().source,
                                services::StatusNotify::Forwarded,
                                report.reason,
                                assertion.0,
                            )
                            .await;
                    }
                    if let Some(assertion) = report.delivered {
                        service
                            .on_status_notify(
                                &report.bundle_id,
                                &bundle.id().source,
                                services::StatusNotify::Delivered,
                                report.reason,
                                assertion.0,
                            )
                            .await;
                    }
                    if let Some(assertion) = report.deleted {
                        service
                            .on_status_notify(
                                &report.bundle_id,
                                &bundle.id().source,
                                services::StatusNotify::Deleted,
                                report.reason,
                                assertion.0,
                            )
                            .await;
                    }

                    // Just delete the bundle, there's no required counters or reporting
                    self.delete_bundle(bundle).await;
                } else {
                    // Park under the canonical registration EID — the exact
                    // key poll_service_waiting matches on registration — with
                    // the same foreign-EID fallback shape as deliver_bundle
                    // (a report about another node's bundle never matches a
                    // local registration either way).
                    let service = self
                        .node_ids
                        .local_service_eid(&report.bundle_id.source)
                        .unwrap_or_else(|| report.bundle_id.source.clone());
                    let desired = bundle::BundleStatus::WaitingForService { service };

                    // Conditional: the reaper can resolve the bundle at any
                    // await, and the park must not resurrect a tombstone.
                    if self.store.swap_status(&mut bundle, &desired).await {
                        self.store.watch_bundle(bundle).await;
                    }
                }
            }
        }
    }
}
