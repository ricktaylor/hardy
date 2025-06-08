use super::*;
use hardy_bpv7::{
    dtn_time::DtnTime,
    status_report::{AdministrativeRecord, BundleStatusReport, ReasonCode, StatusAssertion},
};

impl Dispatcher {
    #[instrument(skip(self))]
    pub(super) async fn report_bundle_reception(
        &self,
        bundle: &bundle::Bundle,
        reason: ReasonCode,
    ) -> Result<(), Error> {
        trace!("Bundle {:?} received", &bundle.bundle.id);

        // Check if a report is requested
        if !bundle.bundle.flags.receipt_report_requested {
            return Ok(());
        }

        trace!("Reporting bundle reception to {}", &bundle.bundle.report_to);

        self.dispatch_status_report(
            hardy_cbor::encode::emit(&AdministrativeRecord::BundleStatusReport(
                BundleStatusReport {
                    bundle_id: bundle.bundle.id.clone(),
                    received: Some(StatusAssertion(if bundle.bundle.flags.report_status_time {
                        if let Some(t) = bundle.metadata.received_at {
                            Some(t.try_into()?)
                        } else {
                            None
                        }
                    } else {
                        None
                    })),
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
        bundle: &bundle::Bundle,
    ) -> Result<(), Error> {
        trace!("Bundle {:?} forwarded", &bundle.bundle.id);

        // Check if a report is requested
        if !bundle.bundle.flags.forward_report_requested {
            return Ok(());
        }

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
            )),
            &bundle.bundle.report_to,
        )
        .await
    }

    #[instrument(skip(self))]
    pub(super) async fn report_bundle_delivery(
        &self,
        bundle: &bundle::Bundle,
    ) -> Result<(), Error> {
        trace!("Bundle {:?} delivered", &bundle.bundle.id);

        // Check if a report is requested
        if !bundle.bundle.flags.delivery_report_requested {
            return Ok(());
        }

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
            )),
            &bundle.bundle.report_to,
        )
        .await
    }

    #[instrument(skip(self))]
    pub async fn report_bundle_deletion(
        &self,
        bundle: &bundle::Bundle,
        reason: ReasonCode,
    ) -> Result<(), Error> {
        trace!("Bundle {:?} deleted", &bundle.bundle.id);

        // Check if a report is requested
        if !bundle.bundle.flags.delete_report_requested {
            return Ok(());
        }

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
            )),
            &bundle.bundle.report_to,
        )
        .await
    }

    #[instrument(skip_all)]
    pub(super) async fn dispatch_status_report(
        &self,
        payload: Vec<u8>,
        report_to: &Eid,
    ) -> Result<(), Error> {
        // Check reports are enabled
        if !self.status_reports {
            return Ok(());
        }

        // Build the bundle
        let mut b = hardy_bpv7::builder::Builder::new();
        b.flags(hardy_bpv7::bundle::Flags {
            is_admin_record: true,
            ..Default::default()
        })
        .source(self.node_ids.get_admin_endpoint(report_to))
        .destination(report_to.clone())
        .add_payload_block(payload);
        let (bundle, data) = b.build();

        // Store to store
        let metadata = self
            .store
            .store(&bundle, data.into(), BundleStatus::default(), None)
            .await?
            .trace_expect("Duplicate bundle generated by builder!");

        // Put bundle into channel
        self.dispatch_task(Task::Dispatch(bundle::Bundle { metadata, bundle }))
            .await;
        Ok(())
    }
}
