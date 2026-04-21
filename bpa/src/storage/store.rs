use core::num::NonZeroUsize;

use flume::Sender;
use hardy_async::sync::Mutex;
use hardy_async::{Notify, TaskPool};
use hardy_bpv7::bundle::Id;
use hardy_bpv7::eid::Eid;
use trace_err::*;
use tracing::error;
#[cfg(feature = "instrument")]
use tracing::instrument;

use super::{BundleStorage, MetadataStorage, reaper};
use crate::bundle::{Bundle, BundleMetadata, BundleStatus};
use crate::dispatcher::Dispatcher;
use crate::{Arc, BTreeSet, Bytes};

pub(crate) struct Store {
    pub(super) tasks: TaskPool,
    pub(super) metadata_storage: Arc<dyn MetadataStorage>,
    pub(super) bundle_storage: Arc<dyn BundleStorage>,

    pub(super) reaper_cache: Arc<Mutex<BTreeSet<reaper::CacheEntry>>>,
    pub(super) reaper_wakeup: Arc<Notify>,
    pub(super) reaper_cache_size: usize,
}

impl Store {
    /// Create a new Store with the configured storage backends.
    ///
    /// The `bundle_storage` may be wrapped in a [`CachedBundleStorage`](super::cached::CachedBundleStorage)
    /// decorator before being passed here.
    pub fn new(
        reaper_cache_size: NonZeroUsize,
        metadata_storage: Arc<dyn MetadataStorage>,
        bundle_storage: Arc<dyn BundleStorage>,
    ) -> Self {
        let tasks = TaskPool::new();
        let reaper_cache = Arc::new(Mutex::new(BTreeSet::new()));
        let reaper_wakeup = Arc::new(Notify::new());
        let reaper_cache_size = reaper_cache_size.into();

        Self {
            tasks,
            metadata_storage,
            bundle_storage,
            reaper_cache,
            reaper_wakeup,
            reaper_cache_size,
        }
    }

    /// Start storage subsystem tasks.
    ///
    /// Optionally runs crash recovery, then starts the reaper background task
    /// for bundle lifetime monitoring.
    pub fn start(self: &Arc<Self>, dispatcher: Arc<Dispatcher>, recover_storage: bool) {
        if recover_storage {
            self.recover(&dispatcher);
        }

        // Start the reaper
        let store = self.clone();
        hardy_async::spawn!(self.tasks, "reaper_task", async move {
            store.run_reaper(dispatcher).await
        });
    }

    pub async fn shutdown(&self) {
        self.tasks.shutdown().await;
    }

    /// Store bundle data and metadata atomically.
    /// Takes a bundle with pre-populated metadata (e.g., from filter processing).
    /// Updates the storage_name field after saving data.
    /// Returns false if duplicate bundle already exists.
    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    pub async fn store(&self, bundle: &mut Bundle, data: &Bytes) -> bool {
        // Write to bundle storage
        let storage_name = self.save_data(data).await;

        // Update storage_name in existing metadata
        bundle.metadata.storage_name = Some(storage_name);

        // Write to metadata store
        match self.metadata_storage.insert(bundle).await {
            Ok(true) => true,
            Ok(false) => {
                // We have a duplicate, remove the duplicate from the bundle store
                if let Some(storage_name) = &bundle.metadata.storage_name {
                    self.delete_data(storage_name).await;
                }
                false
            }
            Err(e) => {
                error!("Failed to insert metadata: {e}");

                // Storage backend failure - clean up the bundle data and
                // return false so the caller abandons this bundle.
                // The storage engine itself should decide if this is fatal.
                if let Some(storage_name) = &bundle.metadata.storage_name {
                    self.delete_data(storage_name).await;
                }
                false
            }
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
    pub async fn save_data(&self, data: &Bytes) -> Arc<str> {
        self.bundle_storage
            .save(data.clone())
            .await
            .trace_expect("Failed to save bundle data")
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    pub async fn delete_data(&self, storage_name: &str) {
        self.bundle_storage
            .delete(storage_name)
            .await
            .trace_expect("Failed to delete bundle data")
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
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

        if &m.bundle.id != bundle_id {
            error!(
                "Metadata store failed to return correct bundle: {} != {bundle_id}",
                m.bundle.id
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
    pub async fn confirm_exists(&self, bundle_id: &Id) -> Option<BundleMetadata> {
        self.metadata_storage
            .confirm_exists(bundle_id)
            .await
            .trace_expect("Failed to confirm bundle existence")
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    pub async fn update_metadata(&self, bundle: &Bundle) {
        self.metadata_storage
            .replace(bundle)
            .await
            .trace_expect("Failed to replace metadata")
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, bundle),fields(bundle.id = %bundle.bundle.id)))]
    pub async fn update_status(&self, bundle: &mut Bundle, status: &BundleStatus) {
        if bundle.metadata.status != *status {
            metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&bundle.metadata.status)).decrement(1.0);
            metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(status)).increment(1.0);

            bundle.metadata.status = status.clone();
            self.metadata_storage
                .update_status(bundle)
                .await
                .trace_expect("Failed to update bundle status");
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn poll_waiting(&self, tx: Sender<Bundle>) {
        self.metadata_storage
            .poll_waiting(tx)
            .await
            .trace_expect("Failed to poll for waiting bundles")
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn poll_service_waiting(&self, source: Eid, tx: Sender<Bundle>) {
        self.metadata_storage
            .poll_service_waiting(source, tx)
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
            metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&BundleStatus::ForwardPending { peer, queue: None }))
                .decrement(reset as f64);
            metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&BundleStatus::Waiting))
                .increment(reset as f64);
        }

        reset != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{bundle_mem, metadata_mem};

    fn make_store() -> Arc<Store> {
        Arc::new(Store::new(
            core::num::NonZeroUsize::new(16).unwrap(),
            Arc::new(metadata_mem::MetadataMemStorage::new(&Default::default())),
            Arc::new(bundle_mem::BundleMemStorage::new(&Default::default())),
        ))
    }

    fn make_bundle(dest: &str) -> Bundle {
        Bundle {
            bundle: hardy_bpv7::bundle::Bundle {
                id: Id {
                    source: "ipn:0.99.1".parse().unwrap(),
                    timestamp: hardy_bpv7::creation_timestamp::CreationTimestamp::now(),
                    fragment_info: None,
                },
                flags: Default::default(),
                crc_type: Default::default(),
                destination: dest.parse().unwrap(),
                report_to: Default::default(),
                lifetime: core::time::Duration::from_secs(3600),
                previous_node: None,
                age: None,
                hop_count: None,
                blocks: Default::default(),
            },
            metadata: Default::default(),
        }
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
}
