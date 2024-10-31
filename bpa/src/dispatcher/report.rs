use super::*;

impl Dispatcher {
    #[instrument(skip(self))]
    pub(super) async fn report_bundle_reception(
        &self,
        bundle: &metadata::Bundle,
        reason: bpv7::StatusReportReasonCode,
    ) -> Result<(), Error> {
        // Check if a report is requested
        if !bundle.bundle.flags.receipt_report_requested {
            return Ok(());
        }

        trace!("Reporting bundle reception to {}", &bundle.bundle.report_to);

        self.dispatch_status_report(
            cbor::encode::emit(&bpv7::AdministrativeRecord::BundleStatusReport(
                bpv7::BundleStatusReport {
                    bundle_id: bundle.bundle.id.clone(),
                    received: Some(bpv7::StatusAssertion(
                        if bundle.bundle.flags.report_status_time {
                            if let Some(t) = bundle.metadata.received_at {
                                Some(t.try_into()?)
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
            )),
            &bundle.bundle.report_to,
        )
        .await
    }

    #[instrument(skip(self))]
    pub(super) async fn report_bundle_forwarded(
        &self,
        bundle: &metadata::Bundle,
    ) -> Result<(), Error> {
        // Check if a report is requested
        if !bundle.bundle.flags.forward_report_requested {
            return Ok(());
        }

        trace!(
            "Reporting bundle as forwarded to {}",
            &bundle.bundle.report_to
        );

        self.dispatch_status_report(
            cbor::encode::emit(&bpv7::AdministrativeRecord::BundleStatusReport(
                bpv7::BundleStatusReport {
                    bundle_id: bundle.bundle.id.clone(),
                    forwarded: Some(bpv7::StatusAssertion(
                        bundle
                            .bundle
                            .flags
                            .report_status_time
                            .then(bpv7::DtnTime::now),
                    )),
                    ..Default::default()
                },
            )),
            &bundle.bundle.report_to,
        )
        .await
    }

    #[instrument(skip(self))]
    pub(super) async fn report_bundle_delivery(
        &self,
        bundle: &metadata::Bundle,
    ) -> Result<(), Error> {
        // Check if a report is requested
        if !bundle.bundle.flags.delivery_report_requested {
            return Ok(());
        }

        trace!("Reporting bundle delivery to {}", &bundle.bundle.report_to);

        // Create a bundle report
        self.dispatch_status_report(
            cbor::encode::emit(&bpv7::AdministrativeRecord::BundleStatusReport(
                bpv7::BundleStatusReport {
                    bundle_id: bundle.bundle.id.clone(),
                    delivered: Some(bpv7::StatusAssertion(
                        bundle
                            .bundle
                            .flags
                            .report_status_time
                            .then(bpv7::DtnTime::now),
                    )),
                    ..Default::default()
                },
            )),
            &bundle.bundle.report_to,
        )
        .await
    }

    #[instrument(skip(self))]
    pub async fn report_bundle_deletion(
        &self,
        bundle: &metadata::Bundle,
        reason: bpv7::StatusReportReasonCode,
    ) -> Result<(), Error> {
        // Check if a report is requested
        if !bundle.bundle.flags.delete_report_requested {
            return Ok(());
        }

        trace!("Reporting bundle deletion to {}", &bundle.bundle.report_to);

        // Create a bundle report
        self.dispatch_status_report(
            cbor::encode::emit(&bpv7::AdministrativeRecord::BundleStatusReport(
                bpv7::BundleStatusReport {
                    bundle_id: bundle.bundle.id.clone(),
                    deleted: Some(bpv7::StatusAssertion(
                        bundle
                            .bundle
                            .flags
                            .report_status_time
                            .then(bpv7::DtnTime::now),
                    )),
                    reason,
                    ..Default::default()
                },
            )),
            &bundle.bundle.report_to,
        )
        .await
    }

    #[instrument(skip_all)]
    pub(super) async fn dispatch_status_report(
        &self,
        payload: Vec<u8>,
        report_to: &bpv7::Eid,
    ) -> Result<(), Error> {
        // Check reports are enabled
        if !self.config.status_reports {
            return Ok(());
        }

        // Don't report to ourselves
        if self.config.admin_endpoints.is_admin_endpoint(report_to) {
            return Ok(());
        }

        // Build the bundle
        let (bundle, data) = bpv7::Builder::new()
            .flags(bpv7::BundleFlags {
                is_admin_record: true,
                ..Default::default()
            })
            .source(self.config.admin_endpoints.get_admin_endpoint(report_to))
            .destination(report_to.clone())
            .add_payload_block(payload)
            .build();

        // Store to store
        let metadata = self
            .store
            .store(
                &bundle,
                data.into(),
                metadata::BundleStatus::default(),
                None,
            )
            .await?
            .trace_expect("Duplicate bundle generated by builder!");

        // Put bundle into channel
        self.dispatch_bundle(metadata::Bundle { metadata, bundle })
            .await
    }
}
