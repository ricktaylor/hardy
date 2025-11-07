use super::*;
use hardy_bpv7::status_report::{AdministrativeRecord, StatusAssertion};

impl Dispatcher {
    #[cfg_attr(feature = "tracing", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    pub(super) async fn administrative_bundle(
        self: &Arc<Self>,
        bundle: &bundle::Bundle,
    ) -> dispatch::DispatchResult {
        // This is a bundle for an Admin Endpoint
        if !bundle.bundle.flags.is_admin_record {
            debug!(
                "Received a bundle for an administrative endpoint that isn't marked as an administrative record"
            );
            return dispatch::DispatchResult::Drop(Some(ReasonCode::BlockUnintelligible));
        }

        let Some(data) = self.load_data(bundle).await else {
            // Bundle data was deleted sometime during processing
            return dispatch::DispatchResult::Gone;
        };

        let payload = match bundle.bundle.decrypt_block(1, &data, self.key_store()) {
            Err(hardy_bpv7::Error::InvalidBPSec(hardy_bpv7::bpsec::Error::NoValidKey)) => {
                // TODO: We are unable to decrypt the payload, what do we do?
                return dispatch::DispatchResult::Wait;
            }
            Err(e) => {
                debug!("Received an invalid administrative record: {e}");
                return dispatch::DispatchResult::Drop(Some(ReasonCode::BlockUnintelligible));
            }
            Ok(hardy_bpv7::bundle::Payload::Range(range)) => data.slice(range),
            Ok(hardy_bpv7::bundle::Payload::Owned(data)) => Bytes::from_owner(data),
        };

        match hardy_cbor::decode::parse(&payload) {
            Err(e) => {
                debug!("Failed to parse administrative record: {e}");
                dispatch::DispatchResult::Drop(Some(ReasonCode::BlockUnintelligible))
            }
            Ok(AdministrativeRecord::BundleStatusReport(report)) => {
                // TODO:  This needs to move to a storage::channel

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
                dispatch::DispatchResult::Drop(None)
            }
        }
    }
}
