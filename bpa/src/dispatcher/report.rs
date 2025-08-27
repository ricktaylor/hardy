use super::*;
use hardy_bpv7::{
    dtn_time::DtnTime,
    status_report::{AdministrativeRecord, BundleStatusReport, ReasonCode, StatusAssertion},
};

impl Dispatcher {
    #[cfg_attr(feature = "tracing", instrument(skip(self, bundle)))]
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
                                bundle.metadata.received_at.try_into().ok()
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

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
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

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
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

    #[cfg_attr(feature = "tracing", instrument(skip(self, bundle)))]
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

    #[cfg_attr(feature = "tracing", instrument(skip(self, payload)))]
    async fn dispatch_status_report(self: &Arc<Self>, payload: Box<[u8]>, report_to: &Eid) {
        // Check reports are enabled
        if self.status_reports {
            let bundle = loop {
                // Build the bundle
                let mut b = hardy_bpv7::builder::Builder::new(
                    self.node_ids.get_admin_endpoint(report_to),
                    report_to.clone(),
                );
                b.with_flags(hardy_bpv7::bundle::Flags {
                    is_admin_record: true,
                    ..Default::default()
                });

                let (bundle, data) = b.build(&payload);

                // Store to store
                match self.store.store(bundle, data.into()).await {
                    Err(e) => {
                        error!("Failed to store status report: {e}");
                        return;
                    }
                    Ok(Some(bundle)) => break bundle,
                    Ok(None) => {
                        // Duplicate bundle generated by builder
                        warn!("Duplicate bundle generated by builder");
                    }
                }
            };

            // Dispatch the new bundle
            let dispatcher = self.clone();
            let span = tracing::trace_span!("parent: None", "dispatch_status_report_task");
            span.follows_from(tracing::Span::current());
            self.task_tracker.spawn(
                async move {
                    if let Ok(forward::ForwardResult::Keep) = dispatcher
                        .forward_bundle_inner(&bundle)
                        .await
                        .inspect_err(|e| error!("Failed to send status report: {e}"))
                    {
                        return;
                    }

                    // Delete the bundle from the bundle store
                    if let Some(storage_name) = &bundle.metadata.storage_name {
                        _ = dispatcher.store.delete_data(storage_name).await;
                    }
                    _ = dispatcher.store.tombstone_metadata(&bundle.bundle.id).await;
                }
                .instrument(span),
            );
        }
    }
}
