use super::*;
use hardy_bpv7::{
    dtn_time::DtnTime,
    status_report::{AdministrativeRecord, BundleStatusReport, ReasonCode, StatusAssertion},
};

impl Dispatcher {
    #[cfg_attr(feature = "tracing", instrument(skip(self, bundle),fields(bundle.id = %bundle.bundle.id)))]
    pub(super) async fn report_bundle_reception(
        self: &Arc<Self>,
        bundle: &bundle::Bundle,
        reason: ReasonCode,
    ) {
        debug!("Bundle {:?} received", &bundle.bundle.id);

        // Check if a report is requested
        if bundle.bundle.flags.receipt_report_requested {
            debug!("Reporting bundle reception to {}", &bundle.bundle.report_to);

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
                .0
                .into(),
                &bundle.bundle.report_to,
            )
            .await
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    pub(super) async fn report_bundle_forwarded(self: &Arc<Self>, bundle: &bundle::Bundle) {
        debug!("Bundle {:?} forwarded", &bundle.bundle.id);

        // Check if a report is requested
        if bundle.bundle.flags.forward_report_requested {
            debug!(
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
                .0
                .into(),
                &bundle.bundle.report_to,
            )
            .await
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    pub(super) async fn report_bundle_delivery(self: &Arc<Self>, bundle: &bundle::Bundle) {
        debug!("Bundle {:?} delivered", &bundle.bundle.id);

        // Check if a report is requested
        if bundle.bundle.flags.delivery_report_requested {
            debug!("Reporting bundle delivery to {}", &bundle.bundle.report_to);

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
                .0
                .into(),
                &bundle.bundle.report_to,
            )
            .await
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self, bundle),fields(bundle.id = %bundle.bundle.id)))]
    pub async fn report_bundle_deletion(
        self: &Arc<Self>,
        bundle: &bundle::Bundle,
        reason: ReasonCode,
    ) {
        debug!("Bundle {:?} deleted", &bundle.bundle.id);

        // Check if a report is requested
        if bundle.bundle.flags.delete_report_requested {
            debug!("Reporting bundle deletion to {}", &bundle.bundle.report_to);

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
                .0
                .into(),
                &bundle.bundle.report_to,
            )
            .await
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self, payload),fields(report_to = %report_to)))]
    async fn dispatch_status_report(self: &Arc<Self>, payload: Box<[u8]>, report_to: &Eid) {
        // Check reports are enabled
        if self.status_reports {
            let mut bundle = loop {
                // Build the bundle
                let (bundle, data) = hardy_bpv7::builder::Builder::new(
                    self.node_ids.get_admin_endpoint(report_to),
                    report_to.clone(),
                )
                .with_flags(hardy_bpv7::bundle::Flags {
                    is_admin_record: true,
                    ..Default::default()
                })
                .add_extension_block(hardy_bpv7::block::Type::Payload)
                .with_flags(hardy_bpv7::block::Flags {
                    delete_bundle_on_failure: true,
                    ..Default::default()
                })
                .build(&payload)
                .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now());

                // Store to store
                if let Some(bundle) = self.store.store(bundle, data.into()).await {
                    break bundle;
                }

                // Duplicate bundle generated by builder
                warn!("Duplicate bundle generated by builder");
            };

            // Dispatch the new bundle
            let dispatcher = self.clone();
            let task = async move {
                match dispatcher.process_bundle(&mut bundle).await {
                    dispatch::DispatchResult::Gone => {}
                    dispatch::DispatchResult::Forward(peer) => {
                        dispatcher.cla_registry.forward(peer, bundle).await;
                    }
                    dispatch::DispatchResult::Wait => {
                        dispatcher.store.watch_bundle(bundle).await;
                    }
                    _ => {
                        // Delete the bundle from the bundle store
                        _ = dispatcher.delete_bundle(bundle).await;
                    }
                }
            };

            #[cfg(feature = "tracing")]
            let task = {
                let span = tracing::trace_span!(parent: None, "dispatch_status_report_task");
                span.follows_from(tracing::Span::current());
                task.instrument(span)
            };

            self.task_tracker.spawn(task);
        }
    }
}
