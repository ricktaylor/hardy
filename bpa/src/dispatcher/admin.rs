use super::*;
use hardy_bpv7::status_report::{AdministrativeRecord, StatusAssertion};

impl Dispatcher {
    #[instrument(level = "trace", skip_all)]
    pub(super) async fn administrative_bundle(
        self: &Arc<Self>,
        bundle: &bundle::Bundle,
    ) -> Result<dispatch::DispatchResult, Error> {
        // This is a bundle for an Admin Endpoint
        if !bundle.bundle.flags.is_admin_record {
            trace!(
                "Received a bundle for an administrative endpoint that isn't marked as an administrative record"
            );
            return Ok(dispatch::DispatchResult::Drop(Some(
                ReasonCode::BlockUnintelligible,
            )));
        }

        let Some(data) = self.load_data(bundle).await? else {
            // Bundle data was deleted sometime during processing - this is benign
            return Ok(dispatch::DispatchResult::Drop(Some(
                ReasonCode::DepletedStorage,
            )));
        };

        match hardy_cbor::decode::parse(&data) {
            Err(e) => {
                trace!("Failed to parse administrative record: {e}");
                Ok(dispatch::DispatchResult::Drop(Some(
                    ReasonCode::BlockUnintelligible,
                )))
            }
            Ok(AdministrativeRecord::BundleStatusReport(report)) => {
                // Find a live service to notify
                if let Some(service) = self.service_registry.find(&report.bundle_id.source).await {
                    // Notify the service
                    let bundle_id = bundle.bundle.id.to_key();

                    let on_status_notify = |assertion: Option<StatusAssertion>, code| async {
                        if let Some(assertion) = assertion {
                            service
                                .service
                                .on_status_notify(&bundle_id, code, report.reason, assertion.0)
                                .await
                        }
                    };

                    on_status_notify(report.received, service::StatusNotify::Received).await;
                    on_status_notify(report.forwarded, service::StatusNotify::Forwarded).await;
                    on_status_notify(report.delivered, service::StatusNotify::Delivered).await;
                    on_status_notify(report.deleted, service::StatusNotify::Deleted).await;
                }
                Ok(dispatch::DispatchResult::Drop(None))
            }
        }
    }
}
