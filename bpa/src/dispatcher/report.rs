use super::*;
use hardy_bpv7::status_report::{
    AdministrativeRecord, BundleStatusReport, ReasonCode, StatusAssertion,
};

use crate::bundle::parse::ReceptionReport;

impl Dispatcher {
    // The cheap universal gates, checked before any report is built:
    // reporting enabled (off by default, §5.1), and a real report-to
    // endpoint — a null report-to requests nothing, and is reachable on the
    // §5.6 Step-4 block-flag path where no bundle-level request exists.
    fn reportable(&self, bundle: &hardy_bpv7::Bundle) -> bool {
        self.status_reports && !bundle.primary.report_to.is_null()
    }

    /// Report a bundle's reception and, for a rejected bundle, its deletion —
    /// in **one** status report: an RFC 9171 §6.1.1 admin record carries
    /// every assertion slot, so one report bundle does the work of two (the
    /// same coalescing ION performs). Each assertion is gated on its own
    /// request flag: `reception` carries the §5.6 facts — the reception
    /// reason, and whether a block's `report_on_failure` flag
    /// [demands](ReceptionReport::demanded) the report regardless of the
    /// bundle-level receipt flag (Step 4's block-flag-alone trigger) — and a
    /// `deletion` reason adds the §5.10 assertion and takes over the
    /// record's one reason-code slot: the deletion is the material event.
    ///
    /// Takes the parsed primary parts directly: a rejected bundle never
    /// becomes a `bundle::Bundle` record.
    #[cfg_attr(feature = "instrument", instrument(skip(self, bundle),fields(bundle.id = %bundle.primary.id)))]
    pub(super) async fn report_bundle_reception(
        &self,
        bundle: &hardy_bpv7::Bundle,
        received_at: time::OffsetDateTime,
        reception: ReceptionReport,
        deletion: Option<ReasonCode>,
    ) {
        debug!("Bundle {} received", bundle.primary.id);

        if !self.reportable(bundle) {
            return;
        }
        let flags = &bundle.primary.flags;
        let received = flags.receipt_report_requested || reception.demanded();
        let deletion = if flags.delete_report_requested {
            deletion
        } else {
            None
        };
        if !received && deletion.is_none() {
            return;
        }

        debug!(
            "Reporting bundle reception to {}",
            &bundle.primary.report_to
        );
        if received {
            metrics::counter!("bpa.status_report.sent", "type" => "reception").increment(1);
        }
        if deletion.is_some() {
            metrics::counter!("bpa.status_report.sent", "type" => "deletion").increment(1);
        }

        self.dispatch_status_report(
            hardy_cbor::encode::emit(&AdministrativeRecord::BundleStatusReport(
                BundleStatusReport {
                    bundle_id: bundle.primary.id.clone(),
                    received: received
                        .then(|| StatusAssertion(flags.report_status_time.then_some(received_at))),
                    deleted: deletion.map(|_| {
                        StatusAssertion(
                            flags.report_status_time.then(time::OffsetDateTime::now_utc),
                        )
                    }),
                    reason: deletion.unwrap_or_else(|| reception.reason()),
                    ..Default::default()
                },
            ))
            .0,
            &bundle.primary.report_to,
        )
        .await
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.id())))]
    pub(super) async fn report_bundle_forwarded(&self, bundle: &bundle::Bundle) {
        debug!("Bundle {} forwarded", bundle.id());

        // Check if a report is requested
        if self.reportable(&bundle.bpv7) && bundle.primary().flags.forward_report_requested {
            debug!(
                "Reporting bundle as forwarded to {}",
                &bundle.primary().report_to
            );
            metrics::counter!("bpa.status_report.sent", "type" => "forwarding").increment(1);

            self.dispatch_status_report(
                hardy_cbor::encode::emit(&AdministrativeRecord::BundleStatusReport(
                    BundleStatusReport {
                        bundle_id: bundle.id().clone(),
                        forwarded: Some(StatusAssertion(
                            bundle
                                .primary()
                                .flags
                                .report_status_time
                                .then(time::OffsetDateTime::now_utc),
                        )),
                        ..Default::default()
                    },
                ))
                .0,
                &bundle.primary().report_to,
            )
            .await
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.id())))]
    pub(super) async fn report_bundle_delivery(&self, bundle: &bundle::Bundle) {
        debug!("Bundle {} delivered", bundle.id());

        // Check if a report is requested
        if self.reportable(&bundle.bpv7) && bundle.primary().flags.delivery_report_requested {
            debug!(
                "Reporting bundle delivery to {}",
                &bundle.primary().report_to
            );
            metrics::counter!("bpa.status_report.sent", "type" => "delivery").increment(1);

            // Create a bundle report
            self.dispatch_status_report(
                hardy_cbor::encode::emit(&AdministrativeRecord::BundleStatusReport(
                    BundleStatusReport {
                        bundle_id: bundle.id().clone(),
                        delivered: Some(StatusAssertion(
                            bundle
                                .primary()
                                .flags
                                .report_status_time
                                .then(time::OffsetDateTime::now_utc),
                        )),
                        ..Default::default()
                    },
                ))
                .0,
                &bundle.primary().report_to,
            )
            .await
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, bundle),fields(bundle.id = %bundle.id())))]
    pub async fn report_bundle_deletion(&self, bundle: &bundle::Bundle, reason: ReasonCode) {
        // Check if a report is requested
        if self.reportable(&bundle.bpv7) && bundle.primary().flags.delete_report_requested {
            debug!(
                "Reporting bundle deletion to {}",
                &bundle.primary().report_to
            );
            metrics::counter!("bpa.status_report.sent", "type" => "deletion").increment(1);

            // Create a bundle report
            self.dispatch_status_report(
                hardy_cbor::encode::emit(&AdministrativeRecord::BundleStatusReport(
                    BundleStatusReport {
                        bundle_id: bundle.id().clone(),
                        deleted: Some(StatusAssertion(
                            bundle
                                .primary()
                                .flags
                                .report_status_time
                                .then(time::OffsetDateTime::now_utc),
                        )),
                        reason,
                        ..Default::default()
                    },
                ))
                .0,
                &bundle.primary().report_to,
            )
            .await
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, payload),fields(report_to = %report_to)))]
    // Callers gate on `reportable` before paying to build `payload`.
    async fn dispatch_status_report(&self, payload: Vec<u8>, report_to: &Eid) {
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

        let data = Bytes::from(data);
        let extracted = crate::bundle::parse::extract_from_built(&bundle, &data)
            .trace_expect("Failed to extract extension fields from built bundle");

        // Wrap in bundle::Bundle with Dispatching status — status reports
        // are internally generated, so they skip both the Originate and
        // Ingress filters and go directly to routing.
        let mut metadata = bundle::BundleMetadata::originated();
        metadata.extensions = extracted;
        let mut bundle = bundle::Bundle {
            bpv7: bundle,
            metadata,
            status: bundle::BundleStatus::Dispatching,
        };

        // Store (no Originate filter - not user-originated)
        if !self.store.store(&mut bundle, &data).await {
            // Duplicate status report - shouldn't happen but handle gracefully
            debug!("Duplicate status report bundle");
            return;
        }

        metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&bundle.status)).increment(1.0);

        // Dispatch via queue to avoid blocking the CLA session reader.
        // Running inline would block incoming bundles on this connection
        // for the duration of the status report's full pipeline.
        self.dispatch_bundle(bundle).await
    }
}
