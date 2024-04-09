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
    store: store::Store,
    status_reports: bool,
    tx: Sender<(bundle::Metadata, bundle::Bundle)>,
    source_eid: bundle::Eid,
}

impl Clone for Dispatcher {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            status_reports: self.status_reports,
            tx: self.tx.clone(),
            source_eid: self.source_eid.clone(),
        }
    }
}

impl Dispatcher {
    pub fn new(
        config: &config::Config,
        store: store::Store,
        task_set: &mut tokio::task::JoinSet<()>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<Self, anyhow::Error> {
        // Load NodeId from config
        let source_eid = match config.get::<String>("node_id") {
            Ok(source_eid) => match source_eid.parse() {
                Ok(source_eid) => source_eid,
                Err(e) => return Err(anyhow!("Invalid \"node_id\" in configuration: {}", e)),
            },
            Err(e) => return Err(anyhow!("Missing \"node_id\" from configuration: {}", e)),
        };

        // Confirm we have a valid EID with administrative endpoint service number
        let source_eid = match source_eid {
            bundle::Eid::Ipn3 {
                allocator_id: _,
                node_number: _,
                service_number: 0,
            } => source_eid,
            bundle::Eid::Dtn {
                node_name: _,
                demux: _,
            } => source_eid,
            e => return Err(anyhow!("Invalid \"node_id\" in configuration: {}", e)),
        };
        log::info!("Local Node ID: {source_eid}");

        // Create a channel for bundles
        let (tx, rx) = channel(16);
        let dispatcher = Self {
            store,
            status_reports: settings::get_with_default(config, "status_reports", false)?,
            tx,
            source_eid,
        };

        // Spawn a bundle receiver
        let dispatcher_cloned = dispatcher.clone();
        task_set
            .spawn(async move { Self::pipeline_pump(dispatcher_cloned, rx, cancel_token).await });

        Ok(dispatcher)
    }

    pub async fn enqueue_bundle(
        &self,
        metadata: bundle::Metadata,
        bundle: bundle::Bundle,
    ) -> Result<(), anyhow::Error> {
        // Put bundle into channel
        self.tx.send((metadata, bundle)).await.map_err(|e| e.into())
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
                            dispatcher.process_bundle(metadata,bundle).await.log_expect("Failed to process bundle");
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

    async fn process_bundle(
        &self,
        metadata: bundle::Metadata,
        bundle: bundle::Bundle,
    ) -> Result<(), anyhow::Error> {
        // This is the meat of the dispatch pipeline
        todo!()
    }

    pub async fn report_bundle_reception(
        &self,
        metadata: &bundle::Metadata,
        bundle: &bundle::Bundle,
        reason: BundleStatusReportReasonCode,
    ) -> Result<(), anyhow::Error> {
        // Check if a report is requested
        if !self.status_reports || !bundle.flags.receipt_report_requested {
            return Ok(());
        }

        // Create a bundle report
        let (bundle, data) = bundle::BundleBuilder::new()
            .is_admin_record(true)
            .source(self.source_eid.clone())
            .destination(bundle.report_to.clone())
            .add_payload_block(new_bundle_status_report(
                metadata, bundle, reason, None, None, None,
            ))
            .build();

        // Store to store
        let metadata = self
            .store
            .store(&bundle, data, bundle::BundleStatus::DispatchPending)
            .await?;

        // And queue it up
        self.enqueue_bundle(metadata, bundle).await
    }

    pub async fn report_bundle_deletion(
        &self,
        metadata: &bundle::Metadata,
        bundle: &bundle::Bundle,
        reason: BundleStatusReportReasonCode,
    ) -> Result<(), anyhow::Error> {
        // Check if a report is requested
        if !self.status_reports || !bundle.flags.delete_report_requested {
            return Ok(());
        }

        // Create a bundle report
        let (bundle, data) = bundle::BundleBuilder::new()
            .is_admin_record(true)
            .source(self.source_eid.clone())
            .destination(bundle.report_to.clone())
            .add_payload_block(new_bundle_status_report(
                metadata,
                bundle,
                reason,
                None,
                None,
                Some(time::OffsetDateTime::now_utc()),
            ))
            .build();

        // Calculate hash
        let hash = sha2::Sha256::digest(&data);

        // Write to bundle storage
        let storage_name = self.bundle_storage.store(data).await?;

        // Write to metadata store
        match self
            .metadata_storage
            .store(status, &storage_name, &hash, bundle)
            .await
        {
            Err(e) => {
                // This is just bad, we can't really claim to have received the bundle,
                // so just cleanup and get out
                let _ = self.bundle_storage.remove(&storage_name).await;
                Err(e)
            }
            Ok(r) => Ok(r),
        }

        // Store to store
        let metadata = self
            .store
            .store(&bundle, data, bundle::BundleStatus::DispatchPending)
            .await?;

        // And queue it up
        self.enqueue_bundle(metadata, bundle).await
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
            if bundle.flags.report_status_time
                && bundle.flags.receipt_report_requested
                && metadata.received_at.is_some()
            {
                cbor::encode::emit([
                    cbor::encode::emit(true),
                    cbor::encode::emit(bundle::dtn_time(&metadata.received_at.unwrap())),
                ])
            } else {
                cbor::encode::emit([cbor::encode::emit(bundle.flags.receipt_report_requested)])
            },
            // Report node forwarded the bundle
            if bundle.flags.report_status_time
                && bundle.flags.forward_report_requested
                && forwarded.is_some()
            {
                cbor::encode::emit([
                    cbor::encode::emit(true),
                    cbor::encode::emit(bundle::dtn_time(&forwarded.unwrap())),
                ])
            } else {
                cbor::encode::emit([cbor::encode::emit(
                    bundle.flags.forward_report_requested && forwarded.is_some(),
                )])
            },
            // Report node delivered the bundle
            if bundle.flags.report_status_time
                && bundle.flags.delivery_report_requested
                && delivered.is_some()
            {
                cbor::encode::emit([
                    cbor::encode::emit(true),
                    cbor::encode::emit(bundle::dtn_time(&delivered.unwrap())),
                ])
            } else {
                cbor::encode::emit([cbor::encode::emit(
                    bundle.flags.delivery_report_requested && delivered.is_some(),
                )])
            },
            // Report node deleted the bundle
            if bundle.flags.report_status_time
                && bundle.flags.delete_report_requested
                && deleted.is_some()
            {
                cbor::encode::emit([
                    cbor::encode::emit(true),
                    cbor::encode::emit(bundle::dtn_time(&deleted.unwrap())),
                ])
            } else {
                cbor::encode::emit([cbor::encode::emit(
                    bundle.flags.delete_report_requested && deleted.is_some(),
                )])
            },
        ]),
        // Reason code
        cbor::encode::emit(reason as u64),
        // Source EID
        cbor::encode::emit(&bundle.id.source),
        // Creation Timestamp
        cbor::encode::emit(&bundle.id.timestamp),
    ];
    if let Some(fragment_info) = &bundle.id.fragment_info {
        // Add fragment info
        report.push(cbor::encode::emit(fragment_info.offset));
        report.push(cbor::encode::emit(fragment_info.total_len));
    }
    cbor::encode::emit([cbor::encode::emit(1u8), cbor::encode::emit(report)])
}
