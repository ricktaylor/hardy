use hardy_bpv7::status_report::{AdministrativeRecord, ReasonCode};
use tracing::debug;
#[cfg(feature = "instrument")]
use tracing::instrument;

use super::Bpa;
use crate::bundle::{Bundle, BundleStatus, Stored};
use crate::rib;
use crate::services;

impl Bpa {
    #[cfg_attr(feature = "instrument", instrument(skip_all, fields(bundle.id = %bundle.bundle.id)))]
    pub(crate) async fn administrative_bundle(&self, mut bundle: Bundle<Stored>) {
        metrics::counter!("bpa.admin_record.received").increment(1);

        if !bundle.bundle.flags.is_admin_record {
            debug!(
                "Received a bundle for an administrative endpoint that isn't marked as an administrative record"
            );
            metrics::counter!("bpa.admin_record.unknown").increment(1);
            self.dispatcher
                .report_bundle_deletion(&bundle, ReasonCode::BlockUnintelligible)
                .await;
            bundle.delete(&self.store).await;
            return;
        }

        let Some(data) = bundle.get_data(&self.store).await else {
            debug!("Bundle data missing from storage");
            bundle.delete(&self.store).await;
            return;
        };

        let payload_result = {
            let key_source = self.keys_registry.key_source(&bundle.bundle, &data);
            bundle.payload(&data, &*key_source)
        }; // key_source dropped here, before any await

        let data = match payload_result {
            Err(hardy_bpv7::Error::InvalidBPSec(hardy_bpv7::bpsec::Error::NoKey)) => {
                // TODO: We are unable to decrypt the payload, what do we do?
                return self.store.watch_bundle(bundle).await;
            }
            Err(e) => {
                debug!("Received an invalid administrative record: {e}");
                self.dispatcher
                    .report_bundle_deletion(&bundle, ReasonCode::BlockUnintelligible)
                    .await;
                bundle.delete(&self.store).await;
                return;
            }
            Ok(data) => data,
        };

        match hardy_cbor::decode::parse(data.as_ref()) {
            Err(e) => {
                debug!("Failed to parse administrative record: {e}");
                metrics::counter!("bpa.admin_record.unknown").increment(1);
                self.dispatcher
                    .report_bundle_deletion(&bundle, ReasonCode::BlockUnintelligible)
                    .await;
                bundle.delete(&self.store).await;
            }
            Ok(AdministrativeRecord::BundleStatusReport(report)) => {
                debug!("Received administrative record: {report:?}");

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

                match self.rib.find_local(&report.bundle_id.source) {
                    Some(rib::FindResult::Deliver(Some(service))) => {
                        if let Some(assertion) = report.received {
                            service
                                .on_status_notify(
                                    &report.bundle_id,
                                    &bundle.bundle.id.source,
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
                                    &bundle.bundle.id.source,
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
                                    &bundle.bundle.id.source,
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
                                    &bundle.bundle.id.source,
                                    services::StatusNotify::Deleted,
                                    report.reason,
                                    assertion.0,
                                )
                                .await;
                        }

                        bundle.delete(&self.store).await;
                    }
                    Some(_) => {
                        // TODO: This match case can be removed when we fix Service registration
                        bundle.delete(&self.store).await;
                    }
                    None => {
                        let status = BundleStatus::WaitingForService {
                            service: report.bundle_id.source.clone(),
                        };
                        bundle.transition(&self.store, status).await;
                        self.store.watch_bundle(bundle).await;
                    }
                }
            }
        }
    }
}
