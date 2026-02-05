use super::*;
use hardy_bpv7::status_report::{AdministrativeRecord, StatusAssertion};

impl Dispatcher {
    #[cfg_attr(feature = "tracing", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    pub(super) async fn administrative_bundle(&self, bundle: bundle::Bundle, data: Bytes) {
        // This is a bundle for an Admin Endpoint
        if !bundle.bundle.flags.is_admin_record {
            debug!(
                "Received a bundle for an administrative endpoint that isn't marked as an administrative record"
            );
            return self
                .drop_bundle(bundle, Some(ReasonCode::BlockUnintelligible))
                .await;
        }

        let payload_result = {
            let key_source = self.key_source(&bundle.bundle, &data);
            bundle.bundle.block_data(1, &data, &*key_source)
        }; // key_source dropped here, before any await

        let data = match payload_result {
            Err(hardy_bpv7::Error::InvalidBPSec(hardy_bpv7::bpsec::Error::NoKey)) => {
                // TODO: We are unable to decrypt the payload, what do we do?
                return self.store.watch_bundle(bundle).await;
            }
            Err(e) => {
                debug!("Received an invalid administrative record: {e}");
                return self
                    .drop_bundle(bundle, Some(ReasonCode::BlockUnintelligible))
                    .await;
            }
            Ok(data) => data,
        };

        match hardy_cbor::decode::parse(data.as_ref()) {
            Err(e) => {
                debug!("Failed to parse administrative record: {e}");
                self.drop_bundle(bundle, Some(ReasonCode::BlockUnintelligible))
                    .await
            }
            Ok(AdministrativeRecord::BundleStatusReport(report)) => {
                // TODO:  This needs to move to a storage::channel
                debug!("Received administrative record: {report:?}");

                // Find a live service to notify
                if let Some(service) = self.service_registry.find(&report.bundle_id.source).await {
                    // Notify the service
                    let on_status_notify = |assertion: Option<StatusAssertion>, code| async {
                        if let Some(assertion) = assertion {
                            service
                                .on_status_notify(
                                    &report.bundle_id,
                                    &bundle.bundle.id.source,
                                    code,
                                    report.reason,
                                    assertion.0,
                                )
                                .await
                        }
                    };

                    on_status_notify(report.received, services::StatusNotify::Received).await;
                    on_status_notify(report.forwarded, services::StatusNotify::Forwarded).await;
                    on_status_notify(report.delivered, services::StatusNotify::Delivered).await;
                    on_status_notify(report.deleted, services::StatusNotify::Deleted).await;
                }
                self.drop_bundle(bundle, None).await
            }
        }
    }
}
