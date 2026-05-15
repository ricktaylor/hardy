use hardy_bpv7::eid::Eid;
use hardy_bpv7::status_report::{
    AdministrativeRecord, BundleStatusReport, ReasonCode, StatusAssertion,
};
use tracing::debug;

use super::Bpa;
use crate::bundle;

#[cfg(feature = "instrument")]
use tracing::instrument;

impl Bpa {
    #[allow(dead_code)]
    #[cfg_attr(feature = "instrument", instrument(skip(self, bundle),fields(bundle.id = %bundle.bundle.id)))]
    pub(super) async fn report_bundle_reception(
        &self,
        bundle: &bundle::Bundle,
        reason: ReasonCode,
    ) {
        debug!("Bundle {} received", bundle.bundle.id);

        if bundle.bundle.flags.receipt_report_requested {
            debug!("Reporting bundle reception to {}", &bundle.bundle.report_to);
            ::metrics::counter!("bpa.status_report.sent", "type" => "reception").increment(1);

            Box::pin(self.report(
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
            ))
            .await
        }
    }

    #[allow(dead_code)]
    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    pub(super) async fn report_bundle_forwarded(&self, bundle: &bundle::Bundle) {
        debug!("Bundle {} forwarded", bundle.bundle.id);

        if bundle.bundle.flags.forward_report_requested {
            debug!(
                "Reporting bundle as forwarded to {}",
                &bundle.bundle.report_to
            );
            ::metrics::counter!("bpa.status_report.sent", "type" => "forwarding").increment(1);

            Box::pin(self.report(
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
            ))
            .await
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    pub(super) async fn report_bundle_delivery(&self, bundle: &bundle::Bundle) {
        debug!("Bundle {} delivered", bundle.bundle.id);

        if bundle.bundle.flags.delivery_report_requested {
            debug!("Reporting bundle delivery to {}", &bundle.bundle.report_to);
            ::metrics::counter!("bpa.status_report.sent", "type" => "delivery").increment(1);

            Box::pin(self.report(
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
            ))
            .await
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, bundle),fields(bundle.id = %bundle.bundle.id)))]
    pub async fn report_bundle_deletion(&self, bundle: &bundle::Bundle, reason: ReasonCode) {
        if bundle.bundle.flags.delete_report_requested {
            debug!("Reporting bundle deletion to {}", &bundle.bundle.report_to);
            ::metrics::counter!("bpa.status_report.sent", "type" => "deletion").increment(1);

            Box::pin(self.report(
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
            ))
            .await
        }
    }
}
