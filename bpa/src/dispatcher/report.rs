use super::*;
use hardy_bpv7::{
    dtn_time::DtnTime,
    status_report::{AdministrativeRecord, BundleStatusReport, ReasonCode, StatusAssertion},
};

impl Dispatcher {
    #[instrument(skip(self))]
    pub(super) async fn report_bundle_reception(
        self: &Arc<Self>,
        bundle: &bundle::Bundle,
        reason: ReasonCode,
    ) {
        trace!("Bundle {:?} received", &bundle.bundle.id);

        // Check if a report is requested
        if bundle.bundle.flags.receipt_report_requested {
            trace!("Reporting bundle reception to {}", &bundle.bundle.report_to);

            self.dispatch_status_report(
                hardy_cbor::encode::emit(&AdministrativeRecord::BundleStatusReport(
                    BundleStatusReport {
                        bundle_id: bundle.bundle.id.clone(),
                        received: Some(StatusAssertion(
                            if bundle.bundle.flags.report_status_time {
                                if let Some(t) = bundle.metadata.received_at {
                                    t.try_into().ok()
                                } else {
                                    None
                                }
                            } else {
                                None
                            },
                        )),
                        reason,
                        ..Default::default()
                    },
                ))
                .into(),
                &bundle.bundle.report_to,
            )
            .await
        }
    }

    #[instrument(skip(self))]
    pub(super) async fn report_bundle_forwarded(self: &Arc<Self>, bundle: &bundle::Bundle) {
        trace!("Bundle {:?} forwarded", &bundle.bundle.id);

        // Check if a report is requested
        if bundle.bundle.flags.forward_report_requested {
            trace!(
                "Reporting bundle as forwarded to {}",
                &bundle.bundle.report_to
            );

            self.dispatch_status_report(
                hardy_cbor::encode::emit(&AdministrativeRecord::BundleStatusReport(
                    BundleStatusReport {
                        bundle_id: bundle.bundle.id.clone(),
                        forwarded: Some(StatusAssertion(
                            bundle.bundle.flags.report_status_time.then(DtnTime::now),
                        )),
                        ..Default::default()
                    },
                ))
                .into(),
                &bundle.bundle.report_to,
            )
            .await
        }
    }

    #[instrument(skip(self))]
    pub(super) async fn report_bundle_delivery(self: &Arc<Self>, bundle: &bundle::Bundle) {
        trace!("Bundle {:?} delivered", &bundle.bundle.id);

        // Check if a report is requested
        if bundle.bundle.flags.delivery_report_requested {
            trace!("Reporting bundle delivery to {}", &bundle.bundle.report_to);

            // Create a bundle report
            self.dispatch_status_report(
                hardy_cbor::encode::emit(&AdministrativeRecord::BundleStatusReport(
                    BundleStatusReport {
                        bundle_id: bundle.bundle.id.clone(),
                        delivered: Some(StatusAssertion(
                            bundle.bundle.flags.report_status_time.then(DtnTime::now),
                        )),
                        ..Default::default()
                    },
                ))
                .into(),
                &bundle.bundle.report_to,
            )
            .await
        }
    }

    #[instrument(skip(self))]
    pub async fn report_bundle_deletion(
        self: &Arc<Self>,
        bundle: &bundle::Bundle,
        reason: ReasonCode,
    ) {
        trace!("Bundle {:?} deleted", &bundle.bundle.id);

        // Check if a report is requested
        if bundle.bundle.flags.delete_report_requested {
            trace!("Reporting bundle deletion to {}", &bundle.bundle.report_to);

            // Create a bundle report
            self.dispatch_status_report(
                hardy_cbor::encode::emit(&AdministrativeRecord::BundleStatusReport(
                    BundleStatusReport {
                        bundle_id: bundle.bundle.id.clone(),
                        deleted: Some(StatusAssertion(
                            bundle.bundle.flags.report_status_time.then(DtnTime::now),
                        )),
                        reason,
                        ..Default::default()
                    },
                ))
                .into(),
                &bundle.bundle.report_to,
            )
            .await
        }
    }

    #[instrument(skip_all)]
    async fn dispatch_status_report(self: &Arc<Self>, payload: Box<[u8]>, report_to: &Eid) {
        // Check reports are enabled
        if self.status_reports {
            let bundle = loop {
                // Build the bundle
                let mut b = hardy_bpv7::builder::Builder::new();
                b.flags(hardy_bpv7::bundle::Flags {
                    is_admin_record: true,
                    ..Default::default()
                })
                .source(self.node_ids.get_admin_endpoint(report_to))
                .destination(report_to.clone())
                .add_payload_block(&payload);

                let (bundle, data) = b.build();

                // Store to store
                match self.store.store(bundle, data.into(), None).await {
                    Err(e) => {
                        error!("Failed to store status report: {e}");
                        return;
                    }
                    Ok(Some(bundle)) => break bundle,
                    Ok(None) => {
                        // Duplicate bundle generated by builder
                    }
                }
            };

            // Dispatch the new bundle
            let dispatcher = self.clone();
            self.task_tracker.spawn(async move {
                if let Ok(dispatch::DispatchResult::Keep) = dispatcher
                    .dispatch_bundle_inner(&bundle)
                    .await
                    .inspect_err(|e| error!("Failed to send status report: {e}"))
                {
                    return;
                }

                // Delete the bundle from the bundle store
                _ = dispatcher.store.delete_data(&bundle.metadata).await;
                _ = dispatcher.store.remove_metadata(&bundle.bundle.id).await;
            });
        }
    }
}
