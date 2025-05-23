mod admin;
mod dispatch;
mod fragment;
mod ingress;
mod local;
mod report;

use super::*;
use metadata::*;

// I can't make this work with closures
#[allow(clippy::large_enum_variant)]
enum Task {
    Dispatch(bundle::Bundle),
    Wait(bpv7::Eid, bpv7::BundleId, time::OffsetDateTime),
}

impl Task {
    async fn exec(self, dispatcher: Arc<Dispatcher>) -> Result<(), Error> {
        match self {
            Task::Dispatch(bundle) => dispatcher.dispatch_bundle(bundle).await,
            Task::Wait(next_hop, bundle_id, until) => {
                dispatcher.on_bundle_wait(next_hop, bundle_id, until).await
            }
        }
    }
}

type Error = Box<dyn std::error::Error + Send + Sync>;

pub struct Dispatcher {
    cancel_token: tokio_util::sync::CancellationToken,
    store: Arc<store::Store>,
    tx: tokio::sync::mpsc::Sender<Task>,
    service_registry: Arc<service_registry::ServiceRegistry>,
    rib: Arc<rib::Rib>,
    ipn_2_element: Arc<eid_pattern::EidPatternSet>,

    // Config options
    status_reports: bool,
    pub node_ids: node_ids::NodeIds,

    // JoinHandles
    run_handle: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Dispatcher {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: &config::Config,
        store: Arc<store::Store>,
        service_registry: Arc<service_registry::ServiceRegistry>,
        rib: Arc<rib::Rib>,
    ) -> Arc<Self> {
        // Create a channel for bundles
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let dispatcher = Arc::new(Self {
            cancel_token: tokio_util::sync::CancellationToken::new(),
            store,
            tx,
            service_registry,
            rib,
            ipn_2_element: Arc::new(config.ipn_2_element.iter().fold(
                eid_pattern::EidPatternSet::new(),
                |mut acc, e| {
                    acc.insert(e.clone());
                    acc
                },
            )),
            status_reports: config.status_reports,
            node_ids: config.node_ids.clone(),
            run_handle: std::sync::Mutex::new(None),
        });

        // Spawn the dispatch task
        *dispatcher
            .run_handle
            .lock()
            .trace_expect("Lock issue in dispatcher new()") = Some(tokio::spawn(
            dispatcher::Dispatcher::run(dispatcher.clone(), rx),
        ));

        dispatcher
    }

    pub async fn shutdown(&self) {
        self.cancel_token.cancel();

        if let Some(j) = {
            self.run_handle
                .lock()
                .trace_expect("Lock issue in dispatcher shutdown()")
                .take()
        } {
            _ = j.await;
        }
    }

    async fn load_data(&self, bundle: &mut bundle::Bundle) -> Result<Option<Bytes>, Error> {
        // Try to load the data, but treat errors as 'Storage Depleted'
        let storage_name = bundle.metadata.storage_name.as_ref().unwrap();
        if let Some(data) = self.store.load_data(storage_name).await? {
            return Ok(Some(data));
        }

        warn!("Bundle data {storage_name} has gone from storage");

        // Report the bundle has gone
        self.report_bundle_deletion(bundle, bpv7::StatusReportReasonCode::DepletedStorage)
            .await?;

        // Leave a tombstone in the metadata, so we can ignore duplicates
        self.store
            .set_status(
                bundle,
                BundleStatus::Tombstone(time::OffsetDateTime::now_utc()),
            )
            .await
            .map(|_| None)
    }

    #[instrument(skip(self))]
    async fn drop_bundle(
        &self,
        mut bundle: bundle::Bundle,
        reason: Option<bpv7::StatusReportReasonCode>,
    ) -> Result<(), Error> {
        if let Some(reason) = reason {
            self.report_bundle_deletion(&bundle, reason).await?;
        }

        // Leave a tombstone in the metadata, so we can ignore duplicates
        self.store
            .set_status(
                &mut bundle,
                BundleStatus::Tombstone(time::OffsetDateTime::now_utc()),
            )
            .await?;

        // Delete the bundle from the bundle store
        if let Some(storage_name) = bundle.metadata.storage_name {
            self.store.delete_data(&storage_name).await?;
        }

        /* Do not keep Tombstones for our own bundles
         * This is done even after we have set a Tombstone
         * status above to avoid a race
         */
        if self.node_ids.contains(&bundle.bundle.id.source) {
            self.store.delete_metadata(&bundle.bundle.id).await?;
        }
        Ok(())
    }

    #[inline]
    async fn dispatch_task(&self, task: Task) {
        // Put bundle into channel, ignoring errors as the only ones are intentional
        _ = self.tx.send(task).await;
    }

    pub(super) async fn bundle_wait(
        &self,
        next_hop: bpv7::Eid,
        bundle: bundle::Bundle,
        mut until: time::OffsetDateTime,
    ) -> Result<(), Error> {
        until = until.min(bundle.expiry());
        self.dispatch_task(Task::Wait(next_hop, bundle.bundle.id, until))
            .await;
        Ok(())
    }

    async fn on_bundle_wait(
        &self,
        next_hop: bpv7::Eid,
        bundle_id: bpv7::BundleId,
        until: time::OffsetDateTime,
    ) -> Result<(), Error> {
        // Check to see if we should wait at all!
        let duration = until - time::OffsetDateTime::now_utc();
        if !duration.is_positive() {
            let Some(bundle) = self.store.load(&bundle_id).await? else {
                // Bundle data was deleted sometime during processing
                return Ok(());
            };
            return self
                .drop_bundle(
                    bundle,
                    Some(bpv7::StatusReportReasonCode::NoTimelyContactWithNextNodeOnRoute),
                )
                .await;
        }

        match self
            .rib
            .wait_for_route(
                &next_hop,
                tokio::time::Duration::new(
                    duration.whole_seconds() as u64,
                    duration.subsec_nanoseconds() as u32,
                ),
                &self.cancel_token,
            )
            .await
        {
            rib::WaitResult::Cancelled => Ok(()),
            rib::WaitResult::Timeout => {
                let Some(bundle) = self.store.load(&bundle_id).await? else {
                    // Bundle data was deleted sometime during processing
                    return Ok(());
                };
                return self
                    .drop_bundle(
                        bundle,
                        Some(bpv7::StatusReportReasonCode::NoTimelyContactWithNextNodeOnRoute),
                    )
                    .await;
            }
            rib::WaitResult::RouteChange => {
                let Some(bundle) = self.store.load(&bundle_id).await? else {
                    // Bundle data was deleted sometime during processing
                    return Ok(());
                };
                self.dispatch_bundle(bundle).await
            }
        }
    }

    #[instrument(skip_all)]
    async fn run(self: Arc<Dispatcher>, mut rx: tokio::sync::mpsc::Receiver<Task>) {
        // Start the store - this can take a while as the store is walked
        self.store
            .start(self.clone(), self.cancel_token.clone())
            .await;

        // We're going to spawn a bunch of tasks
        let mut task_set = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                task = rx.recv() => {
                    match task {
                        Some(task) => {
                            let self_cloned = self.clone();
                            task_set.spawn(async {
                                if let Err(e) = task.exec(self_cloned).await {
                                    error!("{e}");
                                }
                            });
                        },
                        None => break
                    }
                },
                Some(r) = task_set.join_next(), if !task_set.is_empty() => {
                    r.trace_expect("Task terminated unexpectedly");

                },
                _ = self.cancel_token.cancelled(), if !rx.is_closed() => {
                    // Close the queue, we're done
                    rx.close();
                }
            }
        }

        // Wait for all tasks to finish
        while let Some(r) = task_set.join_next().await {
            r.trace_expect("Task terminated unexpectedly")
        }
    }

    pub fn key_closure(
        &self,
    ) -> impl FnMut(
        &bpv7::Eid,
        bpv7::bpsec::Context,
    ) -> Result<Option<bpv7::bpsec::KeyMaterial>, bpv7::bpsec::Error> {
        |_, _| Ok(None)
    }
}
