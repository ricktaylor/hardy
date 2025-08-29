use super::*;
use hardy_bpv7::status_report::{AdministrativeRecord, StatusAssertion};
use std::ops::Deref;

impl Dispatcher {
    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    pub(super) async fn administrative_bundle(
        self: &Arc<Self>,
        bundle: &bundle::Bundle,
    ) -> Result<forward::ForwardResult, Error> {
        // This is a bundle for an Admin Endpoint
        if !bundle.bundle.flags.is_admin_record {
            trace!(
                "Received a bundle for an administrative endpoint that isn't marked as an administrative record"
            );
            return Ok(forward::ForwardResult::Drop(Some(
                ReasonCode::BlockUnintelligible,
            )));
        }

        let Some(data) = self.load_data(bundle).await? else {
            // Bundle data was deleted sometime during processing - this is benign
            return Ok(forward::ForwardResult::Drop(Some(
                ReasonCode::DepletedStorage,
            )));
        };

        let payload = match bundle.bundle.block_payload(1, &data, self.deref())? {
            None => {
                // TODO: We are unable to decrypt the payload, what do we do?
                return Ok(forward::ForwardResult::Keep);
            }
            Some(hardy_bpv7::bundle::Payload::Range(range)) => data.slice(range),
            Some(hardy_bpv7::bundle::Payload::Owned(data)) => Bytes::from_owner(data),
        };

        match hardy_cbor::decode::parse(&payload) {
            Err(e) => {
                trace!("Failed to parse administrative record: {e}");
                Ok(forward::ForwardResult::Drop(Some(
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
                Ok(forward::ForwardResult::Drop(None))
            }
        }
    }
}
