mod admin;
mod dispatch;
mod fragment;
mod ingress;
mod local;
mod report;

use super::*;
use hardy_bpv7::{eid::Eid, status_report::ReasonCode};
use metadata::*;

// I can't make this work with closures
#[allow(clippy::large_enum_variant)]
enum Task {
    Dispatch(bundle::Bundle),
    Wait(Eid, hardy_bpv7::bundle::Id, time::OffsetDateTime),
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
    task_tracker: tokio_util::task::TaskTracker,
    store: Arc<store::Store>,
    tx: tokio::sync::mpsc::Sender<Task>,
    service_registry: Arc<service_registry::ServiceRegistry>,
    rib: Arc<rib::Rib>,
    ipn_2_element: Arc<hardy_eid_pattern::EidPatternSet>,

    // Config options
    status_reports: bool,
    node_ids: node_ids::NodeIds,
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
            task_tracker: tokio_util::task::TaskTracker::new(),
            store,
            tx,
            service_registry,
            rib,
            ipn_2_element: Arc::new(config.ipn_2_element.iter().fold(
                hardy_eid_pattern::EidPatternSet::new(),
                |mut acc, e| {
                    acc.insert(e.clone());
                    acc
                },
            )),
            status_reports: config.status_reports,
            node_ids: config.node_ids.clone(),
        });

        // Spawn the dispatch task
        dispatcher
            .task_tracker
            .spawn(dispatcher::Dispatcher::run(dispatcher.clone(), rx));

        dispatcher
    }

    pub async fn shutdown(&self) {
        self.cancel_token.cancel();
        self.task_tracker.close();
        self.task_tracker.wait().await;
    }

    async fn load_data(&self, bundle: &mut bundle::Bundle) -> Result<Option<Bytes>, Error> {
        // Try to load the data, but treat errors as 'Storage Depleted'
        let storage_name = bundle.metadata.storage_name.as_ref().unwrap();
        if let Some(data) = self.store.load_data(storage_name).await? {
            return Ok(Some(data));
        }

        warn!("Bundle data {storage_name} has gone from storage");

        // Report the bundle has gone
        self.report_bundle_deletion(bundle, ReasonCode::DepletedStorage)
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
        reason: Option<ReasonCode>,
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
        next_hop: Eid,
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
        next_hop: Eid,
        bundle_id: hardy_bpv7::bundle::Id,
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
                .drop_bundle(bundle, Some(ReasonCode::NoTimelyContactWithNextNodeOnRoute))
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
                    .drop_bundle(bundle, Some(ReasonCode::NoTimelyContactWithNextNodeOnRoute))
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

        loop {
            tokio::select! {
                task = rx.recv() => {
                    match task {
                        Some(task) => {
                            let self_cloned = self.clone();
                            self.task_tracker.spawn(async {
                                if let Err(e) = task.exec(self_cloned).await {
                                    error!("{e}");
                                }
                            });
                        },
                        None => break
                    }
                },
                _ = self.cancel_token.cancelled(), if !rx.is_closed() => {
                    // Close the queue, we're done
                    rx.close();
                }
            }
        }
    }

    pub fn key_closure<'a>(
        &self,
    ) -> impl Fn(
        &Eid,
        hardy_bpv7::bpsec::key::Operation,
    ) -> Result<Option<&'a hardy_bpv7::bpsec::Key>, hardy_bpv7::bpsec::Error> {
        |_, _| Ok(None)
    }
}
