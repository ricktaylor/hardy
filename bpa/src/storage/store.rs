use super::*;

impl Store {
    /// Create a new Store with the configured storage backends.
    ///
    /// Uses in-memory storage if no backends are specified in config.
    pub fn new(config: &config::Config) -> Self {
        // Init pluggable storage engines
        Self {
            tasks: hardy_async::TaskPool::new(),
            metadata_storage: config
                .metadata_storage
                .as_ref()
                .cloned()
                .unwrap_or(metadata_mem::new(&metadata_mem::Config::default())),
            bundle_storage: config
                .bundle_storage
                .as_ref()
                .cloned()
                .unwrap_or(bundle_mem::new(&bundle_mem::Config::default())),
            bundle_cache: hardy_async::sync::spin::Mutex::new(LruCache::new(
                config.storage_config.lru_capacity,
            )),
            reaper_cache: Arc::new(Mutex::new(BTreeSet::new())),
            reaper_wakeup: Arc::new(hardy_async::Notify::new()),
            max_cached_bundle_size: config.storage_config.max_cached_bundle_size.into(),
            reaper_cache_size: config.poll_channel_depth.into(),
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
    #[cfg_attr(feature = "tracing", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    pub async fn store(&self, bundle: &mut bundle::Bundle, data: &Bytes) -> bool {
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

                // This is just bad, we can't really claim to have stored the bundle,
                // so just cleanup and get out
                if let Some(storage_name) = &bundle.metadata.storage_name {
                    self.delete_data(storage_name).await;
                }
                panic!("Failed to insert metadata: {e}");
            }
        }
    }

    /// Load bundle data by storage name (cache-first strategy).
    ///
    /// Checks the LRU cache first (peek without updating order), then falls
    /// back to the bundle storage backend if not cached.
    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    pub async fn load_data(&self, storage_name: &str) -> Option<Bytes> {
        // sync::spin::Mutex::lock() returns guard directly (no Result)
        if let Some(data) = self.bundle_cache.lock().peek(storage_name) {
            return Some(data.clone());
        }

        self.bundle_storage
            .load(storage_name)
            .await
            .trace_expect("Failed to load bundle data")
    }

    /// Save bundle data (persist-first, then cache small bundles).
    ///
    /// Always persists to the bundle storage backend first, then caches
    /// in the LRU if the data size is below `max_cached_bundle_size`.
    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    pub async fn save_data(&self, data: &Bytes) -> Arc<str> {
        let storage_name = self
            .bundle_storage
            .save(data.clone())
            .await
            .trace_expect("Failed to save bundle data");

        if data.len() < self.max_cached_bundle_size {
            // sync::spin::Mutex::lock() returns guard directly (no Result)
            self.bundle_cache
                .lock()
                .put(storage_name.clone(), data.clone());
        }

        storage_name
    }

    /// Delete bundle data from cache and storage backend.
    ///
    /// Removes from the LRU cache first, then deletes from the backend.
    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    pub async fn delete_data(&self, storage_name: &str) {
        // sync::spin::Mutex::lock() returns guard directly (no Result)
        self.bundle_cache.lock().pop(storage_name);

        self.bundle_storage
            .delete(storage_name)
            .await
            .trace_expect("Failed to delete bundle data")
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    pub async fn insert_metadata(&self, bundle: &bundle::Bundle) -> bool {
        self.metadata_storage
            .insert(bundle)
            .await
            .trace_expect("Failed to insert metadata")
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all,fields(bundle.id = %bundle_id)))]
    pub async fn get_metadata(&self, bundle_id: &hardy_bpv7::bundle::Id) -> Option<bundle::Bundle> {
        let m = self
            .metadata_storage
            .get(bundle_id)
            .await
            .trace_expect("Failed to get metadata")?;

        if &m.bundle.id != bundle_id {
            error!(
                "Metadata store failed to return correct bundle: {:?} != {bundle_id:?}",
                m.bundle.id
            );
            None
        } else {
            Some(m)
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all,fields(bundle.id = %bundle_id)))]
    pub async fn tombstone_metadata(&self, bundle_id: &hardy_bpv7::bundle::Id) {
        self.metadata_storage
            .tombstone(bundle_id)
            .await
            .trace_expect("Failed to tombstone metadata")
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all,fields(bundle.id = %bundle_id)))]
    pub async fn confirm_exists(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> Option<metadata::BundleMetadata> {
        self.metadata_storage
            .confirm_exists(bundle_id)
            .await
            .trace_expect("Failed to confirm bundle existence")
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    pub async fn update_metadata(&self, bundle: &bundle::Bundle) {
        self.metadata_storage
            .replace(bundle)
            .await
            .trace_expect("Failed to replace metadata")
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    pub async fn poll_waiting(&self, tx: storage::Sender<bundle::Bundle>) {
        self.metadata_storage
            .poll_waiting(tx)
            .await
            .trace_expect("Failed to poll for waiting bundles")
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    pub async fn reset_peer_queue(&self, peer: u32) -> bool {
        self.metadata_storage
            .reset_peer_queue(peer)
            .await
            .trace_expect("Failed to reset peer queue")
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
