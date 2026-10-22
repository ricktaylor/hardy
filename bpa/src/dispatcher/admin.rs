use super::*;
use hardy_bpv7::status_report::AdministrativeRecord;

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

        let Some((mut bundle, data)) = self.load_data_or_drop(bundle).await else {
            return;
        };

        // The administrative endpoint decrypts the payload as its security
        // acceptor. With no key, park in Waiting so each routing event
        // retries the decrypt (keys can arrive later); the reaper still
        // expires it. A payload that fails to decrypt discards the bundle.
        let data = match self.payload_bytes(data) {
            Ok(data) => data,
            Err(hardy_bpv7::Error::InvalidBPSec(hardy_bpv7::bpsec::Error::NoKey)) => {
                // A lost swap means another resolver got there first.
                if self
                    .store
                    .swap_status(&mut bundle, &bundle::BundleStatus::Waiting)
                    .await
                {
                    return self.store.watch_bundle(bundle).await;
                }
                return;
            }
            Err(e) => {
                debug!("Failed to decrypt an administrative record: {e}");
                return self.drop_bundle(bundle, payload_failure_reason(&e)).await;
            }
        };

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
