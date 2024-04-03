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
        cbor::encode::write(vec![
            // Report node received bundle
            cbor::encode::write(
                if bundle.primary.flags.report_status_time && bundle.metadata.is_some() {
                    vec![
                        cbor::encode::write(true),
                        cbor::encode::write(bundle::dtn_time(
                            &bundle.metadata.as_ref().unwrap().received_at,
                        )),
                    ]
                } else {
                    vec![cbor::encode::write(bundle.metadata.is_some())]
                },
            ),
            // Report node forwarded the bundle
            cbor::encode::write(vec![cbor::encode::write(
                if bundle.primary.flags.report_status_time && forwarded.is_some() {
                    vec![
                        cbor::encode::write(true),
                        cbor::encode::write(bundle::dtn_time(&forwarded.unwrap())),
                    ]
                } else {
                    vec![cbor::encode::write(forwarded.is_some())]
                },
            )]),
            // Report node delivered the bundle
            cbor::encode::write(vec![cbor::encode::write(
                if bundle.primary.flags.report_status_time && delivered.is_some() {
                    vec![
                        cbor::encode::write(true),
                        cbor::encode::write(bundle::dtn_time(&delivered.unwrap())),
                    ]
                } else {
                    vec![cbor::encode::write(delivered.is_some())]
                },
            )]),
            // Report node deleted the bundle
            cbor::encode::write(vec![cbor::encode::write(
                if bundle.primary.flags.report_status_time && deleted.is_some() {
                    vec![
                        cbor::encode::write(true),
                        cbor::encode::write(bundle::dtn_time(&deleted.unwrap())),
                    ]
                } else {
                    vec![cbor::encode::write(deleted.is_some())]
                },
            )]),
        ]),
        // Reason code
        cbor::encode::write(reason as u64),
        // Source EID
        cbor::encode::write(&bundle.primary.source),
        // Creation Timestamp
        cbor::encode::write(bundle.primary.timestamp.0),
        cbor::encode::write(bundle.primary.timestamp.1),
    ];
    if let Some(fragment_info) = &bundle.primary.fragment_info {
        // Add fragment info
        report.push(cbor::encode::write(fragment_info.offset));
        report.push(cbor::encode::write(fragment_info.total_len));
    }

    cbor::encode::write(vec![cbor::encode::write(1u8), cbor::encode::write(report)])
}
