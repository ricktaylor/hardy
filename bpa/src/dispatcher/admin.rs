use super::*;

impl Dispatcher {
    #[instrument(skip(self))]
    pub(super) async fn administrative_bundle(
        &self,
        bundle: &mut metadata::Bundle,
    ) -> Result<DispatchResult, Error> {
        // This is a bundle for an Admin Endpoint
        if !bundle.bundle.flags.is_admin_record {
            trace!("Received a bundle for an administrative endpoint that isn't marked as an administrative record");
            return Ok(DispatchResult::Drop(Some(
                bpv7::StatusReportReasonCode::BlockUnintelligible,
            )));
        }

        let Some(data) = self.load_data(bundle).await? else {
            // Bundle data was deleted sometime during processing - this is benign
            return Ok(DispatchResult::Done);
        };

        match cbor::decode::parse(data.as_ref().as_ref()) {
            Err(e) => {
                trace!("Failed to parse administrative record: {e}");
                Ok(DispatchResult::Drop(Some(
                    bpv7::StatusReportReasonCode::BlockUnintelligible,
                )))
            }
            Ok(bpv7::AdministrativeRecord::BundleStatusReport(report)) => {
                // Check if the report is for a bundle sourced from a local service
                if !self
                    .config
                    .admin_endpoints
                    .is_local_service(&report.bundle_id.source)
                {
                    trace!("Received spurious bundle status report {:?}", report);
                    Ok(DispatchResult::Drop(Some(
                        bpv7::StatusReportReasonCode::DestinationEndpointIDUnavailable,
                    )))
                } else {
                    // Find a live service to notify
                    if let Some(endpoint) = self
                        .app_registry
                        .find_by_eid(&report.bundle_id.source)
                        .await
                    {
                        // Notify the service
                        if let Some(assertion) = report.received {
                            endpoint
                                .status_notify(
                                    &report.bundle_id,
                                    app_registry::StatusKind::Received,
                                    report.reason,
                                    assertion.0.map(|t| t.into()),
                                )
                                .await
                        }
                        if let Some(assertion) = report.forwarded {
                            endpoint
                                .status_notify(
                                    &report.bundle_id,
                                    app_registry::StatusKind::Forwarded,
                                    report.reason,
                                    assertion.0.map(|t| t.into()),
                                )
                                .await
                        }
                        if let Some(assertion) = report.delivered {
                            endpoint
                                .status_notify(
                                    &report.bundle_id,
                                    app_registry::StatusKind::Delivered,
                                    report.reason,
                                    assertion.0.map(|t| t.into()),
                                )
                                .await
                        }
                        if let Some(assertion) = report.deleted {
                            endpoint
                                .status_notify(
                                    &report.bundle_id,
                                    app_registry::StatusKind::Deleted,
                                    report.reason,
                                    assertion.0.map(|t| t.into()),
                                )
                                .await
                        }
                    }
                    Ok(DispatchResult::Drop(None))
                }
            }
        }
    }
}
