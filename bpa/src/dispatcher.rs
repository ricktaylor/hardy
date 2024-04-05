use super::*;
use hardy_cbor as cbor;

pub enum BundleStatusReportReasonCode {
    NoAdditionalInformation = 0,
    LifetimeExpired = 1,
    ForwardedOverUnidirectionalLink = 2,
    TransmissionCanceled = 3,
    DepletedStorage = 4,
    DestinationEndpointIDUnavailable = 5,
    NoKnownRouteToDestinationFromHere = 6,
    NoTimelyContactWithNextNodeOnRoute = 7,
    BlockUnintelligible = 8,
    HopLimitExceeded = 9,
    TrafficPared = 10,
    BlockUnsupported = 11,
    MissingSecurityOperation = 12,
    UnknownSecurityOperation = 13,
    UnexpectedSecurityOperation = 14,
    FailedSecurityOperation = 15,
    ConflictingSecurityOperation = 16,
}

pub struct Dispatcher {
    cache: cache::Cache,
    status_reports: bool,
}

impl Clone for Dispatcher {
    fn clone(&self) -> Self {
        Self {
            cache: self.cache.clone(),
            status_reports: self.status_reports,
        }
    }
}

impl Dispatcher {
    pub fn new(
        config: &config::Config,
        cache: cache::Cache,
        _task_set: &mut tokio::task::JoinSet<()>,
        _cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<Self, anyhow::Error> {
        let dispatcher = Self {
            cache,
            status_reports: settings::get_with_default(config, "status_reports", false)?,
        };

        // Spawn a bundle receiver
        /*let cancel_token_cloned = cancel_token.clone();
        let ingress_cloned = ingress.clone();
        task_set.spawn(async move {
            Self::pipeline_pump(ingress_cloned, rx, cancel_token_cloned).await
        });*/

        Ok(dispatcher)
    }

    pub async fn report_bundle_deletion(
        &self,
        bundle: &bundle::Bundle,
        reason: BundleStatusReportReasonCode,
    ) -> Result<(), anyhow::Error> {
        // Check if a report is requested
        if !self.status_reports || !bundle.primary.flags.delete_report_requested {
            return Ok(());
        }

        // Create a report
        let report = new_bundle_status_report(
            bundle,
            reason,
            None,
            None,
            Some(time::OffsetDateTime::now_utc()),
        );

        // Create a bundle
        let (bundle, data) = bundle::BundleBuilder::new()
            .is_admin_record(true)
            .add_payload_block(report)
            .build();

        todo!()
    }
}

fn new_bundle_status_report(
    bundle: &bundle::Bundle,
    reason: BundleStatusReportReasonCode,
    forwarded: Option<time::OffsetDateTime>,
    delivered: Option<time::OffsetDateTime>,
    deleted: Option<time::OffsetDateTime>,
) -> Vec<u8> {
    let mut report = vec![
        // Statuses
        cbor::encode::emit([
            // Report node received bundle
            if bundle.primary.flags.report_status_time
                && bundle.primary.flags.receipt_report_requested
                && bundle.metadata.is_some()
            {
                cbor::encode::emit([
                    cbor::encode::emit(true),
                    cbor::encode::emit(bundle::dtn_time(
                        &bundle.metadata.as_ref().unwrap().received_at,
                    )),
                ])
            } else {
                cbor::encode::emit([cbor::encode::emit(bundle.metadata.is_some())])
            },
            // Report node forwarded the bundle
            if bundle.primary.flags.report_status_time
                && bundle.primary.flags.forward_report_requested
                && forwarded.is_some()
            {
                cbor::encode::emit([
                    cbor::encode::emit(true),
                    cbor::encode::emit(bundle::dtn_time(&forwarded.unwrap())),
                ])
            } else {
                cbor::encode::emit([cbor::encode::emit(forwarded.is_some())])
            },
            // Report node delivered the bundle
            if bundle.primary.flags.report_status_time
                && bundle.primary.flags.delivery_report_requested
                && delivered.is_some()
            {
                cbor::encode::emit([
                    cbor::encode::emit(true),
                    cbor::encode::emit(bundle::dtn_time(&delivered.unwrap())),
                ])
            } else {
                cbor::encode::emit([cbor::encode::emit(delivered.is_some())])
            },
            // Report node deleted the bundle
            if bundle.primary.flags.report_status_time
                && bundle.primary.flags.delete_report_requested
                && deleted.is_some()
            {
                cbor::encode::emit([
                    cbor::encode::emit(true),
                    cbor::encode::emit(bundle::dtn_time(&deleted.unwrap())),
                ])
            } else {
                cbor::encode::emit([cbor::encode::emit(deleted.is_some())])
            },
        ]),
        // Reason code
        cbor::encode::emit(reason as u64),
        // Source EID
        cbor::encode::emit(&bundle.primary.source),
        // Creation Timestamp
        cbor::encode::emit(bundle.primary.timestamp.0),
        cbor::encode::emit(bundle.primary.timestamp.1),
    ];
    if let Some(fragment_info) = &bundle.primary.fragment_info {
        // Add fragment info
        report.push(cbor::encode::emit(fragment_info.offset));
        report.push(cbor::encode::emit(fragment_info.total_len));
    }
    cbor::encode::emit([cbor::encode::emit(1u8), cbor::encode::emit(report)])
}
