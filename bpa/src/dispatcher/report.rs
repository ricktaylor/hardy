use bytes::Bytes;
use hardy_bpv7::builder::Builder;
use hardy_bpv7::bundle::Flags as BundleFlags;
use hardy_bpv7::creation_timestamp::CreationTimestamp;
use hardy_bpv7::eid::Eid;
use hardy_bpv7::status_report::{
    AdministrativeRecord, BundleStatusReport, ReasonCode, StatusAssertion,
};
use time::OffsetDateTime;
use trace_err::TraceErrResult;

#[cfg(feature = "tracing")]
use crate::instrument;

use super::Dispatcher;
use crate::Vec;
use crate::bundle::{Bundle, BundleMetadata, BundleStatus};

impl Dispatcher {
    #[cfg_attr(feature = "tracing", instrument(skip(self, bundle),fields(bundle.id = %bundle.bundle.id)))]
    pub(super) async fn report_bundle_reception(&self, bundle: &Bundle, reason: ReasonCode) {
        tracing::debug!("Bundle {} received", bundle.bundle.id);

        if bundle.bundle.flags.receipt_report_requested {
            tracing::debug!("Reporting bundle reception to {}", &bundle.bundle.report_to);

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
    pub(super) async fn report_bundle_forwarded(&self, bundle: &Bundle) {
        tracing::debug!("Bundle {} forwarded", bundle.bundle.id);

        if bundle.bundle.flags.forward_report_requested {
            tracing::debug!(
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
                                .then(OffsetDateTime::now_utc),
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
    pub(super) async fn report_bundle_delivery(&self, bundle: &Bundle) {
        tracing::debug!("Bundle {} delivered", bundle.bundle.id);

        if bundle.bundle.flags.delivery_report_requested {
            tracing::debug!("Reporting bundle delivery to {}", &bundle.bundle.report_to);

            self.dispatch_status_report(
                hardy_cbor::encode::emit(&AdministrativeRecord::BundleStatusReport(
                    BundleStatusReport {
                        bundle_id: bundle.bundle.id.clone(),
                        delivered: Some(StatusAssertion(
                            bundle
                                .bundle
                                .flags
                                .report_status_time
                                .then(OffsetDateTime::now_utc),
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
    pub async fn report_bundle_deletion(&self, bundle: &Bundle, reason: ReasonCode) {
        if bundle.bundle.flags.delete_report_requested {
            tracing::debug!("Reporting bundle deletion to {}", &bundle.bundle.report_to);

            self.dispatch_status_report(
                hardy_cbor::encode::emit(&AdministrativeRecord::BundleStatusReport(
                    BundleStatusReport {
                        bundle_id: bundle.bundle.id.clone(),
                        deleted: Some(StatusAssertion(
                            bundle
                                .bundle
                                .flags
                                .report_status_time
                                .then(OffsetDateTime::now_utc),
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
        if self.status_reports {
            // Build the bundle
            let (bundle, data) = Builder::new(
                self.node_ids.get_admin_endpoint(report_to),
                report_to.clone(),
            )
            .with_flags(BundleFlags {
                is_admin_record: true,
                ..Default::default()
            })
            .with_payload(payload.into())
            .build(CreationTimestamp::now())
            .trace_expect("Failed to create new bundle");

            let mut bundle = Bundle {
                metadata: BundleMetadata {
                    status: BundleStatus::New,
                    ..Default::default()
                },
                bundle,
            };

            let data = Bytes::from(data);
            if !self.store.store(&mut bundle, &data).await {
                // Duplicate status report - shouldn't happen but handle gracefully
                tracing::debug!("Duplicate status report bundle");
                return;
            }

            // Just fire the report off now - it ensures sequential reporting (ish)
            Box::pin(self.ingest_bundle_inner(bundle, data)).await
        }
    }
}
