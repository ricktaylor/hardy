use core::num::NonZeroUsize;
use flume::Sender;
use hardy_async::{CancellationToken, TaskPool};
use hardy_bpv7::bundle::Id;
use hardy_bpv7::eid::Eid;

use super::reaper::Reaper;
use super::{BundleStorage, MetadataStorage};
use crate::bundle::{Bundle, BundleStatus, Stored};
use crate::dispatcher::Dispatcher;
use crate::{Arc, Bytes};

/// Facade over the metadata and bundle storage backends.
pub(crate) struct Store {
    pub(crate) tasks: TaskPool,
    pub(crate) metadata_storage: Arc<dyn MetadataStorage>,
    bundle_storage: Arc<dyn BundleStorage>,
    pub(crate) reaper: Reaper,
}

impl Store {
    pub fn new(
        reaper_cache_size: NonZeroUsize,
        metadata_storage: Arc<dyn MetadataStorage>,
        bundle_storage: Arc<dyn BundleStorage>,
    ) -> Self {
        let tasks = TaskPool::new();
        let reaper = Reaper::new(reaper_cache_size.into());

        Self {
            tasks,
            metadata_storage,
            bundle_storage,
            reaper,
        }
    }

    pub fn start(self: &Arc<Self>, dispatcher: Arc<Dispatcher>) {
        let store = self.clone();
        hardy_async::spawn!(self.tasks, "reaper_task", async move {
            store.reaper.run(store.clone(), dispatcher).await
        });
    }

    pub(crate) fn tasks(&self) -> &TaskPool {
        &self.tasks
    }

    pub(crate) fn cancel_token(&self) -> &CancellationToken {
        self.tasks.cancel_token()
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.tasks.is_cancelled()
    }

    pub async fn shutdown(&self) {
        self.tasks.shutdown().await;
    }

    // -- Bundle data operations (delegated to BundleStorage) --

    pub(crate) async fn walk_bundles(
        &self,
        tx: Sender<super::RecoveryResponse>,
    ) -> super::Result<()> {
        self.bundle_storage.walk(tx).await
    }

    pub async fn save_data(&self, data: &Bytes) -> super::Result<Arc<str>> {
        self.bundle_storage.save(data.clone()).await
    }

    pub async fn load_data(&self, storage_name: &str) -> super::Result<Option<Bytes>> {
        self.bundle_storage.load(storage_name).await
    }

    pub async fn overwrite_data(&self, storage_name: &str, data: &Bytes) -> super::Result<()> {
        self.bundle_storage
            .overwrite(storage_name, data.clone())
            .await
    }

    pub async fn delete_data(&self, storage_name: &str) -> super::Result<()> {
        self.bundle_storage.delete(storage_name).await
    }

    // -- Metadata operations (delegated to MetadataStorage) --

    pub async fn insert_metadata(&self, bundle: &Bundle<Stored>) -> super::Result<bool> {
        self.metadata_storage.insert(bundle).await
    }

    pub async fn get_metadata(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> super::Result<Option<Bundle<Stored>>> {
        self.metadata_storage.get(bundle_id).await
    }

    pub async fn update_metadata(&self, bundle: &Bundle<Stored>) -> super::Result<()> {
        self.metadata_storage.replace(bundle).await
    }

    pub async fn update_status(
        &self,
        bundle: &mut Bundle<Stored>,
        status: &BundleStatus,
    ) -> super::Result<()> {
        if bundle.metadata.status != *status {
            metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&bundle.metadata.status)).decrement(1.0);
            metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(status)).increment(1.0);

            bundle.metadata.status = status.clone();
            self.metadata_storage.update_status(bundle).await?;
        }
        Ok(())
    }

    pub async fn tombstone_metadata(&self, bundle_id: &Id) -> super::Result<()> {
        self.metadata_storage.tombstone(bundle_id).await
    }

    // -- Reaper --

    pub fn watch_bundle(&self, bundle: &Bundle<Stored>) {
        self.reaper.watch(bundle, true);
    }

    // -- Recovery --

    pub(crate) async fn mark_unconfirmed(&self) {
        self.metadata_storage.mark_unconfirmed().await;
    }

    pub(crate) async fn confirm_exists(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> super::Result<Option<Bundle<Stored>>> {
        self.metadata_storage.confirm_exists(bundle_id).await
    }

    pub(crate) async fn remove_unconfirmed(
        &self,
        tx: flume::Sender<Bundle<Stored>>,
    ) -> super::Result<()> {
        self.metadata_storage.remove_unconfirmed(tx).await
    }

    // -- Polling --

    pub(crate) async fn poll_adu_fragments(
        &self,
        tx: flume::Sender<Bundle<Stored>>,
        status: &BundleStatus,
    ) -> super::Result<()> {
        self.metadata_storage.poll_adu_fragments(tx, status).await
    }

    pub async fn poll_waiting(&self, tx: flume::Sender<Bundle<Stored>>) -> super::Result<()> {
        self.metadata_storage.poll_waiting(tx).await
    }

    pub async fn poll_service_waiting(
        &self,
        source: Eid,
        tx: flume::Sender<Bundle<Stored>>,
    ) -> super::Result<()> {
        self.metadata_storage.poll_service_waiting(source, tx).await
    }

    pub async fn reset_peer_queue(&self, peer: u32) -> super::Result<bool> {
        let reset = self.metadata_storage.reset_peer_queue(peer).await?;

        if reset > 0 {
            metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&BundleStatus::ForwardPending { peer, queue: None }))
                .decrement(reset as f64);
            metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&BundleStatus::Waiting))
                .increment(reset as f64);
        }
        Ok(reset > 0)
    }
}
