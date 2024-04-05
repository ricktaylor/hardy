use super::*;
use hardy_cbor as cbor;
use tokio::sync::mpsc::*;

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
    tx: Sender<(bundle::Metadata, bundle::Bundle)>,
    source_eid: bundle::Eid,
}

impl Clone for Dispatcher {
    fn clone(&self) -> Self {
        Self {
            cache: self.cache.clone(),
            status_reports: self.status_reports,
            tx: self.tx.clone(),
            source_eid: self.source_eid.clone(),
        }
    }
}

impl Dispatcher {
    pub fn new(
        config: &config::Config,
        cache: cache::Cache,
        task_set: &mut tokio::task::JoinSet<()>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<Self, anyhow::Error> {
        // Create a channel for bundles
        let (tx, rx) = channel(16);
        let dispatcher = Self {
            cache,
            status_reports: settings::get_with_default(config, "status_reports", false)?,
            tx,
        };

        // Spawn a bundle receiver
        let dispatcher_cloned = dispatcher.clone();
        task_set
            .spawn(async move { Self::pipeline_pump(dispatcher_cloned, rx, cancel_token).await });

        Ok(dispatcher)
    }

    async fn pipeline_pump(
        self,
        mut rx: Receiver<(bundle::Metadata, bundle::Bundle)>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        // We're going to spawn a bunch of tasks
        let mut task_set = tokio::task::JoinSet::new();

        loop {
            tokio::select! {
                bundle = rx.recv() => match bundle {
                    None => break,
                    Some((metadata,bundle)) => {
                        let dispatcher = self.clone();
                        task_set.spawn(async move {
                            dispatcher.do_something_with_the_bundle(metadata,bundle).await;
                        });
                    }
                },
                _ = cancel_token.cancelled() => break
            }
        }

        // Wait for all sub-tasks to complete
        while let Some(r) = task_set.join_next().await {
            r.log_expect("Task terminated unexpectedly")
        }
    }

    async fn do_something_with_the_bundle(
        &self,
        metadata: bundle::Metadata,
        bundle: bundle::Bundle,
    ) {
        // This is the meat of the ingress pipeline
        todo!()
    }

    pub async fn report_bundle_reception(
        &self,
        metadata: &bundle::Metadata,
        bundle: &bundle::Bundle,
    ) -> Result<(), anyhow::Error> {
        // Check if a report is requested
        if !self.status_reports || !bundle.primary.flags.receipt_report_requested {
            return Ok(());
        }

        // Create a bundle report
        let (bundle, data) = bundle::BundleBuilder::new()
            .is_admin_record(true)
            .source(self.source_eid.clone())
            .destination(bundle.primary.report_to.clone())
            .add_payload_block(new_bundle_status_report(
                metadata,
                bundle,
                BundleStatusReportReasonCode::NoAdditionalInformation,
                None,
                None,
                None,
            ))
            .build();

        // Store to cache
        let metadata = self
            .cache
            .store(&bundle, data, bundle::BundleStatus::ForwardPending)
            .await?;

        // And queue it up
        self.tx.send((metadata, bundle)).await.map_err(|e| e.into())
    }

    pub async fn report_bundle_deletion(
        &self,
        metadata: &bundle::Metadata,
        bundle: &bundle::Bundle,
        reason: BundleStatusReportReasonCode,
    ) -> Result<(), anyhow::Error> {
        // Check if a report is requested
        if !self.status_reports || !bundle.primary.flags.delete_report_requested {
            return Ok(());
        }

        // Create a bundle report
        let (bundle, data) = bundle::BundleBuilder::new()
            .is_admin_record(true)
            .source(self.source_eid.clone())
            .destination(bundle.primary.report_to.clone())
            .add_payload_block(new_bundle_status_report(
                metadata,
                bundle,
                reason,
                None,
                None,
                Some(time::OffsetDateTime::now_utc()),
            ))
            .build();

        // Store to cache
        let metadata = self
            .cache
            .store(&bundle, data, bundle::BundleStatus::ForwardPending)
            .await?;

        // And queue it up
        self.tx.send((metadata, bundle)).await.map_err(|e| e.into())
    }
}

fn new_bundle_status_report(
    metadata: &bundle::Metadata,
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
                && metadata.received_at.is_some()
            {
                cbor::encode::emit([
                    cbor::encode::emit(true),
                    cbor::encode::emit(bundle::dtn_time(&metadata.received_at.unwrap())),
                ])
            } else {
                cbor::encode::emit([cbor::encode::emit(
                    bundle.primary.flags.receipt_report_requested,
                )])
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
                cbor::encode::emit([cbor::encode::emit(
                    bundle.primary.flags.forward_report_requested && forwarded.is_some(),
                )])
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
                cbor::encode::emit([cbor::encode::emit(
                    bundle.primary.flags.delivery_report_requested && delivered.is_some(),
                )])
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
                cbor::encode::emit([cbor::encode::emit(
                    bundle.primary.flags.delete_report_requested && deleted.is_some(),
                )])
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
