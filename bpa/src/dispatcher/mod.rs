mod collect;
mod config;
mod forward;
mod local;
mod receive;
mod report;

use self::config::Config;
use super::*;
use hardy_cbor as cbor;
use std::sync::Arc;
use tokio::sync::mpsc::*;
use utils::cancel::cancellable_sleep;

pub use local::SendRequest;

pub struct Dispatcher {
    config: Config,
    cancel_token: tokio_util::sync::CancellationToken,
    store: Arc<store::Store>,
    tx: Sender<metadata::Bundle>,
    cla_registry: cla_registry::ClaRegistry,
    app_registry: app_registry::AppRegistry,
    fib: Option<fib::Fib>,
}

impl Dispatcher {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: &::config::Config,
        admin_endpoints: utils::admin_endpoints::AdminEndpoints,
        store: Arc<store::Store>,
        cla_registry: cla_registry::ClaRegistry,
        app_registry: app_registry::AppRegistry,
        fib: Option<fib::Fib>,
        task_set: &mut tokio::task::JoinSet<()>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Arc<Self> {
        // Create a channel for bundles
        let (tx, rx) = channel(16);
        let dispatcher = Arc::new(Self {
            config: Config::new(config, admin_endpoints),
            cancel_token,
            store,
            tx,
            cla_registry,
            app_registry,
            fib,
        });

        // Spawn the pump
        let dispatcher_cloned = dispatcher.clone();
        task_set.spawn(Self::pipeline_pump(dispatcher_cloned, rx));

        dispatcher
    }

    #[instrument(skip_all)]
    async fn pipeline_pump(dispatcher: Arc<Self>, mut rx: Receiver<metadata::Bundle>) {
        // We're going to spawn a bunch of tasks
        let mut task_set = tokio::task::JoinSet::new();

        // Give some feedback
        let timer = tokio::time::sleep(tokio::time::Duration::from_secs(5));
        tokio::pin!(timer);
        let mut bundles_inflight = 0u64;
        let mut bundles_processed = 0u64;

        loop {
            tokio::select! {
                () = &mut timer => {
                    info!("{bundles_processed} bundles processed, {bundles_inflight} bundles in flight");
                    bundles_processed = 0;
                    timer.as_mut().reset(tokio::time::Instant::now() + tokio::time::Duration::from_secs(5));
                },
                bundle = rx.recv() => {
                    let dispatcher = dispatcher.clone();
                    let bundle = bundle.trace_expect("Dispatcher channel unexpectedly closed");

                    bundles_inflight = bundles_inflight.saturating_add(1);

                    task_set.spawn(async move {
                        dispatcher.dispatch_bundle(bundle).await.trace_expect("Failed to dispatch bundle");
                    });
                },
                Some(r) = task_set.join_next(), if !task_set.is_empty() => {
                    r.trace_expect("Task terminated unexpectedly");

                    bundles_inflight -= 1;
                    bundles_processed = bundles_processed.saturating_add(1);
                },
                _ = dispatcher.cancel_token.cancelled() => break
            }
        }

        // Wait for all sub-tasks to complete
        while let Some(r) = task_set.join_next().await {
            r.trace_expect("Task terminated unexpectedly")
        }
    }

    #[inline]
    async fn enqueue_bundle(&self, bundle: metadata::Bundle) -> Result<(), Error> {
        // Put bundle into channel
        self.tx.send(bundle).await.map_err(Into::into)
    }

    #[instrument(skip(self))]
    async fn dispatch_bundle(&self, mut bundle: metadata::Bundle) -> Result<(), Error> {
        loop {
            match bundle.metadata.status {
                metadata::BundleStatus::IngressPending
                | metadata::BundleStatus::ForwardPending
                | metadata::BundleStatus::Tombstone(_) => {
                    unreachable!()
                }
                metadata::BundleStatus::DispatchPending => {
                    // Check if we are the final destination
                    if self
                        .config
                        .admin_endpoints
                        .is_local_service(&bundle.bundle.destination)
                    {
                        if bundle.bundle.id.fragment_info.is_some() {
                            return self.reassemble(bundle).await;
                        } else if self
                            .config
                            .admin_endpoints
                            .is_admin_endpoint(&bundle.bundle.destination)
                        {
                            // The bundle is for the Administrative Endpoint
                            return self.administrative_bundle(bundle).await;
                        } else {
                            // The bundle is ready for collection
                            trace!("Bundle is ready for local delivery");
                            self.store
                                .set_status(&mut bundle, metadata::BundleStatus::CollectionPending)
                                .await?;
                        }
                    } else {
                        // Forward to another BPA
                        return self.forward_bundle(bundle).await;
                    }
                }
                metadata::BundleStatus::ReassemblyPending => {
                    // Wait for other fragments to arrive
                    return Ok(());
                }
                metadata::BundleStatus::CollectionPending => {
                    // Check if we have a local service registered
                    if let Some(endpoint) = self
                        .app_registry
                        .find_by_eid(&bundle.bundle.destination)
                        .await
                    {
                        // Notify that the bundle is ready for collection
                        trace!("Notifying application that bundle is ready for collection");
                        endpoint.collection_notify(&bundle.bundle.id).await;
                    }
                    return Ok(());
                }
                metadata::BundleStatus::ForwardAckPending(_, _) => {
                    // Clear the pending ACK, we are reprocessing
                    self.store
                        .set_status(&mut bundle, metadata::BundleStatus::DispatchPending)
                        .await?;
                }
                metadata::BundleStatus::Waiting(until) => {
                    return self.delay_bundle(bundle, until).await
                }
            }
        }
    }

    #[instrument(skip(self))]
    async fn reassemble(&self, _bundle: metadata::Bundle) -> Result<(), Error> {
        /* Either wait for more fragments to arrive
        self.store.set_status(&mut bundle, metadata::BundleStatus::ReassemblyPending).await?;

        Or

        // TODO: We need to handle the case when the reassembled fragment is larger than our total RAM!
        Reassemble and self.enqueue_bundle()

        */

        warn!("Bundle requires fragment reassembly");
        todo!()
    }

    #[instrument(skip(self))]
    async fn administrative_bundle(&self, bundle: metadata::Bundle) -> Result<(), Error> {
        // This is a bundle for an Admin Endpoint
        if !bundle.bundle.flags.is_admin_record {
            trace!("Received a bundle for an administrative endpoint that isn't marked as an administrative record");
            return self
                .drop_bundle(
                    bundle,
                    Some(bpv7::StatusReportReasonCode::BlockUnintelligible),
                )
                .await;
        }

        let Some(data) = self.load_data(&bundle).await? else {
            // Bundle data was deleted sometime during processing - this is benign
            return Ok(());
        };

        let reason = match cbor::decode::parse::<bpv7::AdministrativeRecord>(data.as_ref().as_ref())
        {
            Err(e) => {
                trace!("Failed to parse administrative record: {e}");
                Some(bpv7::StatusReportReasonCode::BlockUnintelligible)
            }
            Ok(bpv7::AdministrativeRecord::BundleStatusReport(report)) => {
                // Check if the report is for a bundle sourced from a local service
                if !self
                    .config
                    .admin_endpoints
                    .is_local_service(&report.bundle_id.source)
                {
                    trace!("Received spurious bundle status report {:?}", report);
                    Some(bpv7::StatusReportReasonCode::DestinationEndpointIDUnavailable)
                } else {
                    // Find a live service to notify
                    if let Some(endpoint) = self
                        .app_registry
                        .find_by_eid(&report.bundle_id.source)
                        .await
                    {
                        // Notify the service
                        if let Some(assertion) = report.received {
                            endpoint
                                .status_notify(
                                    &report.bundle_id,
                                    app_registry::StatusKind::Received,
                                    report.reason,
                                    assertion.0.map(|t| t.into()),
                                )
                                .await
                        }
                        if let Some(assertion) = report.forwarded {
                            endpoint
                                .status_notify(
                                    &report.bundle_id,
                                    app_registry::StatusKind::Forwarded,
                                    report.reason,
                                    assertion.0.map(|t| t.into()),
                                )
                                .await
                        }
                        if let Some(assertion) = report.delivered {
                            endpoint
                                .status_notify(
                                    &report.bundle_id,
                                    app_registry::StatusKind::Delivered,
                                    report.reason,
                                    assertion.0.map(|t| t.into()),
                                )
                                .await
                        }
                        if let Some(assertion) = report.deleted {
                            endpoint
                                .status_notify(
                                    &report.bundle_id,
                                    app_registry::StatusKind::Deleted,
                                    report.reason,
                                    assertion.0.map(|t| t.into()),
                                )
                                .await
                        }
                    }
                    Some(bpv7::StatusReportReasonCode::NoAdditionalInformation)
                }
            }
        };

        // Done with the bundle
        self.drop_bundle(bundle, reason).await
    }

    async fn load_data(
        &self,
        bundle: &metadata::Bundle,
    ) -> Result<Option<hardy_bpa_api::storage::DataRef>, Error> {
        // Try to load the data, but treat errors as 'Storage Depleted'
        let storage_name = bundle.metadata.storage_name.as_ref().unwrap();
        match self.store.load_data(storage_name).await? {
            None => {
                warn!("Bundle data {storage_name} has gone from storage");

                // Report the bundle has gone
                self.report_bundle_deletion(bundle, bpv7::StatusReportReasonCode::DepletedStorage)
                    .await
                    .map(|_| None)
            }
            Some(data) => Ok(Some(data)),
        }
    }

    pub async fn delay_bundle(
        &self,
        mut bundle: metadata::Bundle,
        until: time::OffsetDateTime,
    ) -> Result<(), Error> {
        // Check if it's worth us waiting (safety check for metadata storage clock drift)
        let wait = until - time::OffsetDateTime::now_utc();
        if wait > time::Duration::new(self.config.wait_sample_interval as i64, 0) {
            // Nothing to do now, it will be picked up later
            trace!("Bundle will wait offline until: {until}");
            return Ok(());
        }

        if let metadata::BundleStatus::ForwardAckPending(_, _) = &bundle.metadata.status {
            trace!("Waiting for bundle forwarding acknowledgement inline until: {until}");

            // Wait a bit
            if !cancellable_sleep(wait, &self.cancel_token).await {
                // Cancelled
                return Ok(());
            }

            // Check if the bundle has been acknowledged while we slept
            let Some(b) = self.store.load(&bundle.bundle.id).await? else {
                // It's not longer waiting, our work here is done
                return Ok(());
            };
            bundle = b;
        } else {
            trace!("Waiting to dispatch bundle inline until: {until}");

            // Wait a bit
            if !cancellable_sleep(wait, &self.cancel_token).await {
                // Cancelled
                return Ok(());
            }

            // Set status to DispatchPending
            self.store
                .set_status(&mut bundle, metadata::BundleStatus::DispatchPending)
                .await?;
        }

        trace!("Dispatching bundle");

        // Put bundle into channel
        self.enqueue_bundle(bundle).await
    }

    #[instrument(skip(self))]
    async fn drop_bundle(
        &self,
        mut bundle: metadata::Bundle,
        reason: Option<bpv7::StatusReportReasonCode>,
    ) -> Result<(), Error> {
        if let Some(reason) = reason {
            self.report_bundle_deletion(&bundle, reason).await?;
        }

        // Leave a tombstone in the metadata, so we can ignore duplicates
        if let metadata::BundleStatus::Tombstone(_) = bundle.metadata.status {
            // Don't update Tombstone timestamp
        } else {
            self.store
                .set_status(
                    &mut bundle,
                    metadata::BundleStatus::Tombstone(time::OffsetDateTime::now_utc()),
                )
                .await?;
        }

        // Delete the bundle from the bundle store
        if let Some(storage_name) = bundle.metadata.storage_name {
            self.store.delete_data(&storage_name).await?;
        }

        /* Do not keep Tombstones for our own bundles
         * This is done even after we have set a Tombstone
         * status above to avoid a race
         */
        if self
            .config
            .admin_endpoints
            .is_admin_endpoint(&bundle.bundle.id.source)
        {
            self.store.delete_metadata(&bundle.bundle.id).await?;
        }
        Ok(())
    }
}
