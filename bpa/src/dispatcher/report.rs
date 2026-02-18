use super::*;
use hardy_bpv7::status_report::{
    AdministrativeRecord, BundleStatusReport, ReasonCode, StatusAssertion,
};

impl Dispatcher {
    #[cfg_attr(feature = "tracing", instrument(skip(self, bundle),fields(bundle.id = %bundle.bundle.id)))]
    pub(super) async fn report_bundle_reception(
        &self,
        bundle: &bundle::Bundle,
        reason: ReasonCode,
    ) {
        debug!("Bundle {} received", bundle.bundle.id);

        // Check if a report is requested
        if bundle.bundle.flags.receipt_report_requested {
            debug!("Reporting bundle reception to {}", &bundle.bundle.report_to);

            self.dispatch_status_report(
                hardy_cbor::encode::emit(&AdministrativeRecord::BundleStatusReport(
                    BundleStatusReport {
                        bundle_id: bundle.bundle.id.clone(),
                        received: Some(StatusAssertion(
                            if bundle.bundle.flags.report_status_time {
                                Some(bundle.metadata.read_only.received_at)
                            } else {
                                None
                            },
                        )),
                        reason,
                        ..Default::default()
                    },
                ))
                .0,
                &bundle.bundle.report_to,
            )
            .await
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    pub(super) async fn report_bundle_forwarded(&self, bundle: &bundle::Bundle) {
        debug!("Bundle {} forwarded", bundle.bundle.id);

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
                            bundle
                                .bundle
                                .flags
                                .report_status_time
                                .then(time::OffsetDateTime::now_utc),
                        )),
                        ..Default::default()
                    },
                ))
                .0,
                &bundle.bundle.report_to,
            )
            .await
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    pub(super) async fn report_bundle_delivery(&self, bundle: &bundle::Bundle) {
        debug!("Bundle {} delivered", bundle.bundle.id);

        // Check if a report is requested
        if bundle.bundle.flags.delivery_report_requested {
            debug!("Reporting bundle delivery to {}", &bundle.bundle.report_to);

            // Create a bundle report
            self.dispatch_status_report(
                hardy_cbor::encode::emit(&AdministrativeRecord::BundleStatusReport(
                    BundleStatusReport {
                        bundle_id: bundle.bundle.id.clone(),
                        delivered: Some(StatusAssertion(
                            bundle
                                .bundle
                                .flags
                                .report_status_time
                                .then(time::OffsetDateTime::now_utc),
                        )),
                        ..Default::default()
                    },
                ))
                .0,
                &bundle.bundle.report_to,
            )
            .await
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self, bundle),fields(bundle.id = %bundle.bundle.id)))]
    pub async fn report_bundle_deletion(&self, bundle: &bundle::Bundle, reason: ReasonCode) {
        // Check if a report is requested
        if bundle.bundle.flags.delete_report_requested {
            debug!("Reporting bundle deletion to {}", &bundle.bundle.report_to);

            // Create a bundle report
            self.dispatch_status_report(
                hardy_cbor::encode::emit(&AdministrativeRecord::BundleStatusReport(
                    BundleStatusReport {
                        bundle_id: bundle.bundle.id.clone(),
                        deleted: Some(StatusAssertion(
                            bundle
                                .bundle
                                .flags
                                .report_status_time
                                .then(time::OffsetDateTime::now_utc),
                        )),
                        reason,
                        ..Default::default()
                    },
                ))
                .0,
                &bundle.bundle.report_to,
            )
            .await
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self, payload),fields(report_to = %report_to)))]
    async fn dispatch_status_report(&self, payload: Vec<u8>, report_to: &Eid) {
        // Check reports are enabled
        if self.status_reports {
            // Build the bundle
            let (bundle, data) = hardy_bpv7::builder::Builder::new(
                self.node_ids.get_admin_endpoint(report_to),
                report_to.clone(),
            )
            .with_flags(hardy_bpv7::bundle::Flags {
                is_admin_record: true,
                ..Default::default()
            })
            .with_payload(payload.into())
            .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
            .trace_expect("Failed to create new bundle");

            // Wrap in bundle::Bundle with initial metadata (not stored yet)
            let mut bundle = bundle::Bundle {
                metadata: metadata::BundleMetadata {
                    status: metadata::BundleStatus::New,
                    ..Default::default()
                },
                bundle,
            };

            // Store (no Originate filter - not user-originated)
            let data = Bytes::from(data);
            if !self.store.store(&mut bundle, &data).await {
                // Duplicate status report - shouldn't happen but handle gracefully
                debug!("Duplicate status report bundle");
                return;
            }

            // Just fire the report off now - it ensures sequential reporting (ish)
            Box::pin(self.ingest_bundle_inner(bundle, data)).await
        }
    }
}
