use super::*;

impl Dispatcher {
    #[instrument(skip(self))]
    pub(super) async fn administrative_bundle(
        &self,
        mut bundle: bundle::Bundle,
    ) -> Result<(), Error> {
        // This is a bundle for an Admin Endpoint
        if !bundle.bundle.flags.is_admin_record {
            trace!(
                "Received a bundle for an administrative endpoint that isn't marked as an administrative record"
            );
            return self
                .drop_bundle(
                    bundle,
                    Some(bpv7::StatusReportReasonCode::BlockUnintelligible),
                )
                .await;
        }

        let Some(data) = self.load_data(&mut bundle).await? else {
            // Bundle data was deleted sometime during processing - this is benign
            return Ok(());
        };

        match cbor::decode::parse(data.as_ref().as_ref()) {
            Err(e) => {
                trace!("Failed to parse administrative record: {e}");
                self.drop_bundle(
                    bundle,
                    Some(bpv7::StatusReportReasonCode::BlockUnintelligible),
                )
                .await
            }
            Ok(bpv7::AdministrativeRecord::BundleStatusReport(report)) => {
                // Find a live service to notify
                if let Some(service) = self.service_registry.find(&report.bundle_id.source).await {
                    // Notify the service
                    let bundle_id = bundle.bundle.id.to_key();

                    let on_status_notify = |assertion: Option<bpv7::StatusAssertion>, code| async {
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
                self.drop_bundle(bundle, None).await
            }
        }
    }
}
