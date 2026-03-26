use super::*;
use crate::storage::error::{Error, Result};

impl Store {
    /// Create a new Store with the configured storage backends.
    /// Uses in-memory storage if no backends are provided.
    pub fn new(
        lru_capacity: Option<core::num::NonZeroUsize>,
        max_cached_bundle_size: core::num::NonZeroUsize,
        reaper_cache_size: core::num::NonZeroUsize,
        metadata_storage: Arc<dyn storage::MetadataStorage>,
        bundle_storage: Arc<dyn storage::BundleStorage>,
    ) -> Self {
        Self {
            tasks: hardy_async::TaskPool::new(),
            metadata_storage,
            bundle_storage,
            bundle_cache: lru_capacity.map(|capacity| storage::BundleCache {
                lru: hardy_async::sync::spin::Mutex::new(LruCache::new(capacity)),
                max_bundle_size: max_cached_bundle_size.into(),
            }),
            reaper_cache: Arc::new(Mutex::new(BTreeSet::new())),
            reaper_wakeup: Arc::new(hardy_async::Notify::new()),
            reaper_cache_size: reaper_cache_size.into(),
        }
    }

    /// Start storage subsystem tasks.
    ///
    /// Optionally runs crash recovery, then starts the reaper background task
    /// for bundle lifetime monitoring.
    pub fn start(self: &Arc<Self>, dispatcher: Arc<dispatcher::Dispatcher>, recover_storage: bool) {
        if recover_storage {
            self.recover(&dispatcher);
        }

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
    /// Returns `Ok(())` on success, `Err(Error::DuplicateBundle)` if a bundle with the same ID
    /// already exists, or another `Err` variant on I/O failure.
    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    pub async fn store(&self, bundle: &mut bundle::Bundle, data: &Bytes) -> Result<()> {
        let storage_name = self.save_data(data).await?;

        bundle.metadata.storage_name = Some(storage_name.clone());

        if let Err(e) = self.metadata_storage.insert(bundle).await {
            self.bundle_storage.delete(storage_name.as_ref()).await;
            return Err(e);
        }
        Ok(())
    }

    /// Load bundle data by storage name (cache-first strategy).
    ///
    /// Checks the LRU cache first (peek without updating order), then falls
    /// back to the bundle storage backend if not cached.
    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    pub async fn load_data(&self, storage_name: &str) -> Result<Option<Bytes>> {
        if let Some(cache) = &self.bundle_cache {
            if let Some(data) = cache.lru.lock().peek(storage_name) {
                return Ok(Some(data.clone()));
            }
        }

        self.bundle_storage.load(storage_name).await
    }

    /// Save bundle data (persist-first, then cache small bundles).
    ///
    /// Always persists to the bundle storage backend first, then caches
    /// in the LRU if the data size is below `max_cached_bundle_size`.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn save_data(&self, data: &Bytes) -> Result<Arc<str>> {
        let storage_name = self.bundle_storage.save(data.clone()).await?;

        if let Some(cache) = &self.bundle_cache {
            if data.len() < cache.max_bundle_size {
                cache.lru.lock().put(storage_name.clone(), data.clone());
            }
        }

        Ok(storage_name)
    }

    /// Delete bundle data from cache and storage backend.
    ///
    /// Removes from the LRU cache first, then deletes from the backend.
    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    pub async fn delete_data(&self, storage_name: &str) -> Result<()> {
        if let Some(cache) = &self.bundle_cache {
            cache.lru.lock().pop(storage_name);
        }

        self.bundle_storage.delete(storage_name).await
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    pub async fn insert_metadata(&self, bundle: &bundle::Bundle) -> Result<()> {
        self.metadata_storage.insert(bundle).await
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle_id)))]
    pub async fn get_metadata(&self, bundle_id: &hardy_bpv7::bundle::Id) -> Result<bundle::Bundle> {
        match self.metadata_storage.get(bundle_id).await? {
            None => Err(Error::BundleNotFound {
                id: bundle_id.clone(),
            }),
            Some(bundle) if &bundle.bundle.id != bundle_id => Err(Error::BundleMismatch {
                expected: bundle_id.clone(),
                found: bundle.bundle.id,
            }),
            Some(bundle) => Ok(bundle),
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle_id)))]
    pub async fn tombstone_metadata(&self, bundle_id: &hardy_bpv7::bundle::Id) -> Result<()> {
        self.metadata_storage.tombstone(bundle_id).await
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle_id)))]
    pub async fn confirm_exists(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> Result<Option<bundle::BundleMetadata>> {
        self.metadata_storage.confirm_exists(bundle_id).await
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    pub async fn update_metadata(&self, bundle: &bundle::Bundle) -> Result<()> {
        self.metadata_storage.replace(bundle).await
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, bundle),fields(bundle.id = %bundle.bundle.id)))]
    pub async fn update_status(
        &self,
        bundle: &mut bundle::Bundle,
        status: bundle::BundleStatus,
    ) -> Result<()> {
        if bundle.metadata.status != status {
            bundle.metadata.status = status;
            self.metadata_storage.update_status(bundle).await?;
        }
        Ok(())
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn poll_waiting(&self, tx: storage::Sender<bundle::Bundle>) -> Result<()> {
        self.metadata_storage.poll_waiting(tx).await
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn poll_service_waiting(
        &self,
        source: Eid,
        tx: storage::Sender<bundle::Bundle>,
    ) -> Result<()> {
        self.metadata_storage.poll_service_waiting(source, tx).await
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn reset_peer_queue(&self, peer: u32) -> Result<bool> {
        self.metadata_storage.reset_peer_queue(peer).await
    }
}

#[cfg(test)]
mod tests {
    // use super::*;

    // // TODO: Implement test for 'Quota Enforcement' (Attempt to store bundle exceeding total capacity)
    // #[test]
    // fn test_quota_enforcement() {
    //     todo!("Verify Attempt to store bundle exceeding total capacity");
    // }

    // // TODO: Implement test for 'Double Delete' (Handle deletion of already removed bundle)
    // #[test]
    // fn test_double_delete() {
    //     todo!("Verify Handle deletion of already removed bundle");
    // }

    // // TODO: Implement test for 'Transaction Rollback' (Verify data cleanup on metadata failure)
    // #[test]
    // fn test_transaction_rollback() {
    //     todo!("Verify data cleanup on metadata failure");
    // }
}
