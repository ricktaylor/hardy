use core::num::NonZeroUsize;

use hardy_async::TaskPool;
use hardy_bpv7::{bundle::Id, eid::Eid};
use trace_err::*;
use tracing::error;
#[cfg(feature = "instrument")]
use tracing::instrument;

use super::{BundleStorage, ConfirmResponse, MetadataStorage, reaper::Reaper};
use crate::{
    Arc, Bytes,
    bundle::{Bundle, BundleStatus},
    dispatcher::Dispatcher,
    stream::Sender,
};

pub struct Store {
    pub(super) tasks: TaskPool,
    pub(super) metadata_storage: Arc<dyn MetadataStorage>,
    pub(super) bundle_storage: Arc<dyn BundleStorage>,
    pub(super) reaper: Arc<Reaper>,
}

impl Store {
    /// Create a new Store with the configured storage backends.
    ///
    /// The `bundle_storage` may be wrapped in a [`CachedBundleStorage`](super::bundle_cache::CachedBundleStorage)
    /// decorator before being passed here.
    pub fn new(
        reaper_cache_size: NonZeroUsize,
        metadata_storage: Arc<dyn MetadataStorage>,
        bundle_storage: Arc<dyn BundleStorage>,
    ) -> Self {
        let tasks = TaskPool::new();
        let reaper = Arc::new(Reaper::new(
            tasks.clone(),
            metadata_storage.clone(),
            reaper_cache_size.into(),
        ));

        Self {
            tasks,
            metadata_storage,
            bundle_storage,
            reaper,
        }
    }

    /// Start storage subsystem tasks.
    ///
    /// Optionally runs crash recovery — awaited to completion, so the store
    /// is quiescent while recovery's checkpoint resets run (see
    /// [`recover`](Self::recover)) — then starts the reaper background task
    /// for bundle lifetime monitoring.
    pub async fn start(self: &Arc<Self>, dispatcher: Arc<Dispatcher>, recover_storage: bool) {
        if recover_storage {
            self.recover(&dispatcher).await;
        }

        let reaper = self.reaper.clone();
        hardy_async::spawn!(self.tasks, "reaper_task", async move {
            reaper.run(dispatcher).await
        });
    }

    pub async fn shutdown(&self) {
        self.tasks.shutdown().await;
    }

    /// Add a bundle to the reaper's expiry watch list.
    pub async fn watch_bundle(&self, bundle: Bundle) {
        self.reaper.watch(&bundle, true);
    }

    /// Store bundle data and metadata atomically.
    /// Takes a bundle with pre-populated metadata (e.g., from filter processing).
    /// Updates the storage_name field after saving data.
    /// Returns false if — and only if — a duplicate bundle already exists:
    /// a backend failure aborts, like every other storage seat, so `false`
    /// can never misreport a storage outage as a duplicate.
    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.id())))]
    pub async fn store(&self, bundle: &mut Bundle, data: &Bytes) -> bool {
        // Write to bundle storage
        let storage_name = self.save_data(data.clone()).await;

        // Update storage_name in existing metadata
        bundle.metadata.storage_name = Some(storage_name);

        // Write to metadata store. A failing backend is a failing disk: the
        // BPA does not limp on against it.
        if self
            .metadata_storage
            .insert(bundle)
            .await
            .trace_expect("Failed to insert metadata")
        {
            true
        } else {
            // We have a duplicate, remove the duplicate from the bundle store
            if let Some(storage_name) = &bundle.metadata.storage_name {
                self.delete_data(storage_name).await;
            }
            false
        }
    }

    /// Load bundle data by storage name (read-through cache).
    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    pub async fn load_data(&self, storage_name: &str) -> Option<Bytes> {
        self.bundle_storage
            .load(storage_name)
            .await
            .trace_expect("Failed to load bundle data")
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn save_data(&self, data: Bytes) -> Arc<str> {
        self.bundle_storage
            .save(data)
            .await
            .trace_expect("Failed to save bundle data")
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, data)))]
    pub async fn replace_data(&self, storage_name: &str, data: Bytes) {
        self.bundle_storage
            .replace(storage_name, data)
            .await
            .trace_expect("Failed to replace bundle data")
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    pub async fn delete_data(&self, storage_name: &str) {
        self.bundle_storage
            .delete(storage_name)
            .await
            .trace_expect("Failed to delete bundle data")
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.id())))]
    pub async fn insert_metadata(&self, bundle: &Bundle) -> bool {
        self.metadata_storage
            .insert(bundle)
            .await
            .trace_expect("Failed to insert metadata")
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle_id)))]
    pub async fn get_metadata(&self, bundle_id: &Id) -> Option<Bundle> {
        let m = self
            .metadata_storage
            .get(bundle_id)
            .await
            .trace_expect("Failed to get metadata")?;

        if m.id() != bundle_id {
            error!(
                "Metadata store failed to return correct bundle: {} != {bundle_id}",
                m.id()
            );
            None
        } else {
            Some(m)
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle_id)))]
    pub async fn tombstone_metadata(&self, bundle_id: &Id) {
        self.metadata_storage
            .tombstone(bundle_id)
            .await
            .trace_expect("Failed to tombstone metadata")
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle_id)))]
    pub async fn confirm_exists(&self, bundle_id: &Id) -> Option<ConfirmResponse> {
        self.metadata_storage
            .confirm_exists(bundle_id)
            .await
            .trace_expect("Failed to confirm bundle existence")
    }

    // Compare-and-swap from the caller's snapshot status: the arbiter for
    // writers racing the peer sweeps, the expiry reaper, and each other.
    // Gauges move only when the swap wins.
    #[cfg_attr(feature = "instrument", instrument(skip(self, bundle),fields(bundle.id = %bundle.id())))]
    pub async fn swap_status(&self, bundle: &mut Bundle, status: &BundleStatus) -> bool {
        let swapped = self
            .metadata_storage
            .swap_status(bundle.id(), &bundle.status, status)
            .await
            .trace_expect("Failed to swap bundle status");

        if swapped {
            metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&bundle.status)).decrement(1.0);
            metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(status)).increment(1.0);

            bundle.status = status.clone();
        }

        swapped
    }

    // Conditional, terminal form of swap_status: tombstones the bundle's
    // metadata only if its status still matches the caller's snapshot. The
    // arbiter for resolutions whose action is the deletion itself — the
    // bundle never transits a status another queue's poller could recover.
    // The caller owns the follow-up data deletion and gauge accounting
    // (delete_bundle tolerates the already-present tombstone).
    #[cfg_attr(feature = "instrument", instrument(skip(self, bundle),fields(bundle.id = %bundle.id())))]
    pub async fn tombstone_if(&self, bundle: &Bundle) -> bool {
        self.metadata_storage
            .tombstone_if(bundle.id(), &bundle.status)
            .await
            .trace_expect("Failed to tombstone bundle metadata")
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn poll_waiting(&self, stream: &dyn Sender<Bundle>) {
        self.metadata_storage
            .poll_waiting(stream)
            .await
            .trace_expect("Failed to poll for waiting bundles")
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn poll_service_waiting(&self, source: Eid, stream: &dyn Sender<Bundle>) {
        self.metadata_storage
            .poll_service_waiting(source, stream)
            .await
            .trace_expect("Failed to poll for waiting bundles")
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn reset_peer_queue(&self, peer: u32) -> bool {
        let reset = self
            .metadata_storage
            .reset_peer_queue(peer)
            .await
            .trace_expect("Failed to reset peer queue");

        if reset > 0 {
            // Label derivation only: the variant selects the label, the
            // fields (including the placeholder adjacency) never reach it.
            metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&BundleStatus::ForwardPending { peer, queue: 0, next_hop: Eid::Null }))
                .decrement(reset as f64);
            metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&BundleStatus::Waiting))
                .increment(reset as f64);
        }

        reset != 0
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn reset_peer_ack_pending(&self, peer: u32) -> bool {
        let reset = self
            .metadata_storage
            .reset_peer_ack_pending(peer)
            .await
            .trace_expect("Failed to reset peer ack-pending transfers");

        if reset > 0 {
            metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&BundleStatus::ForwardAckPending { peer }))
                .decrement(reset as f64);
            metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&BundleStatus::Waiting))
                .increment(reset as f64);
        }

        reset != 0
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn reset_service_queue(&self, service: &Eid) -> bool {
        let reset = self
            .metadata_storage
            .reset_service_queue(service)
            .await
            .trace_expect("Failed to reset service delivery queue");

        if reset > 0 {
            metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&BundleStatus::DeliverPending { service: service.clone() }))
                .decrement(reset as f64);
            metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&BundleStatus::WaitingForService { service: service.clone() }))
                .increment(reset as f64);
        }

        reset != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bundle::tests::test_bundle,
        storage::{Result, bundle_mem, metadata_mem},
    };

    fn make_store() -> Arc<Store> {
        Arc::new(Store::new(
            core::num::NonZeroUsize::new(16).unwrap(),
            Arc::new(metadata_mem::MetadataMemStorage::new(None)),
            Arc::new(bundle_mem::BundleMemStorage::new(None, None)),
        ))
    }

    fn make_bundle(dest: &str) -> Bundle {
        test_bundle("ipn:0.99.1", dest)
    }

    // Store a bundle and then store a duplicate — second insert should return false.
    #[tokio::test]
    async fn test_quota_enforcement() {
        let store = make_store();
        let data = Bytes::from(vec![0xABu8; 100]);
        let mut bundle = make_bundle("ipn:0.2.1");

        // First store should succeed
        assert!(store.store(&mut bundle, &data).await);

        // Same bundle ID again should be rejected (duplicate)
        let mut bundle2 = bundle.clone();
        assert!(
            !store.store(&mut bundle2, &data).await,
            "Duplicate bundle should be rejected"
        );
    }

    // Deleting a bundle that doesn't exist should not panic.
    #[tokio::test]
    async fn test_double_delete() {
        let store = make_store();
        let data = Bytes::from(vec![0xCDu8; 50]);
        let mut bundle = make_bundle("ipn:0.3.1");

        assert!(store.store(&mut bundle, &data).await);

        let storage_name = bundle.metadata.storage_name.as_ref().unwrap().clone();

        // First delete
        store.delete_data(&storage_name).await;

        // Second delete of same name should not panic
        store.delete_data(&storage_name).await;

        // Loading deleted data should return None
        assert!(store.load_data(&storage_name).await.is_none());
    }

    // When metadata insertion fails (duplicate), bundle data should be cleaned up.
    #[tokio::test]
    async fn test_transaction_rollback() {
        let store = make_store();
        let data = Bytes::from(vec![0xEFu8; 75]);
        let mut bundle = make_bundle("ipn:0.4.1");

        // First store succeeds
        assert!(store.store(&mut bundle, &data).await);
        let first_storage_name = bundle.metadata.storage_name.as_ref().unwrap().clone();

        // Second store of same bundle ID fails (duplicate) — the new data should be cleaned up
        let mut bundle2 = bundle.clone();
        bundle2.metadata.storage_name = None; // Reset so store() generates new name
        assert!(!store.store(&mut bundle2, &data).await);

        // Original data should still be accessible
        assert!(store.load_data(&first_storage_name).await.is_some());
    }

    // A metadata-backend failure aborts — it must never surface as the
    // duplicate `false`, which callers are entitled to retry against.
    #[tokio::test]
    #[should_panic(expected = "Failed to insert metadata")]
    async fn metadata_backend_failure_aborts() {
        use hardy_async::async_trait;
        use hardy_bpv7::eid::Eid;

        use crate::stream::Sender;

        struct FailingMetadata;

        #[async_trait]
        impl MetadataStorage for FailingMetadata {
            async fn get(&self, _bundle_id: &Id) -> Result<Option<Bundle>> {
                unimplemented!()
            }
            async fn insert(&self, _bundle: &Bundle) -> Result<bool> {
                Err("backend down".into())
            }
            async fn swap_status(
                &self,
                _bundle_id: &Id,
                _expected: &BundleStatus,
                _status: &BundleStatus,
            ) -> Result<bool> {
                unimplemented!()
            }
            async fn tombstone_if(
                &self,
                _bundle_id: &Id,
                _expected: &BundleStatus,
            ) -> Result<bool> {
                unimplemented!()
            }
            async fn tombstone(&self, _bundle_id: &Id) -> Result<()> {
                unimplemented!()
            }
            async fn start_recovery(&self) {}
            async fn confirm_exists(
                &self,
                _bundle_id: &Id,
            ) -> Result<Option<super::super::ConfirmResponse>> {
                unimplemented!()
            }
            async fn remove_unconfirmed(&self, _stream: &dyn Sender<Bundle>) -> Result<()> {
                unimplemented!()
            }
            async fn reset_peer_queue(&self, _peer: u32) -> Result<u64> {
                unimplemented!()
            }
            async fn reset_peer_ack_pending(&self, _peer: u32) -> Result<u64> {
                unimplemented!()
            }
            async fn reset_service_queue(&self, _service: &Eid) -> Result<u64> {
                unimplemented!()
            }
            async fn poll_expiry(&self, _stream: &dyn Sender<Bundle>) -> Result<()> {
                unimplemented!()
            }
            async fn poll_waiting(&self, _stream: &dyn Sender<Bundle>) -> Result<()> {
                unimplemented!()
            }
            async fn poll_service_waiting(
                &self,
                _source: Eid,
                _stream: &dyn Sender<Bundle>,
            ) -> Result<()> {
                unimplemented!()
            }
            async fn poll_adu_fragments(
                &self,
                _stream: &dyn Sender<Bundle>,
                _status: &BundleStatus,
            ) -> Result<()> {
                unimplemented!()
            }
            async fn poll_pending(
                &self,
                _stream: &dyn Sender<Bundle>,
                _status: &BundleStatus,
                _limit: usize,
            ) -> Result<()> {
                unimplemented!()
            }
        }

        let store = Arc::new(Store::new(
            core::num::NonZeroUsize::new(16).unwrap(),
            Arc::new(FailingMetadata),
            Arc::new(bundle_mem::BundleMemStorage::new(None, None)),
        ));
        let data = Bytes::from(vec![0x11u8; 32]);
        let mut bundle = make_bundle("ipn:0.5.1");

        store.store(&mut bundle, &data).await;
    }
}
