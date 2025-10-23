use super::*;

impl Store {
    pub fn new(config: &config::Config) -> Self {
        // Init pluggable storage engines
        Self {
            cancel_token: tokio_util::sync::CancellationToken::new(),
            task_tracker: tokio_util::task::TaskTracker::new(),
            metadata_storage: config
                .metadata_storage
                .as_ref()
                .map(|s| s.clone())
                .unwrap_or(metadata_mem::new(&metadata_mem::Config::default())),
            bundle_storage: config
                .bundle_storage
                .as_ref()
                .map(|s| s.clone())
                .unwrap_or(bundle_mem::new(&bundle_mem::Config::default())),
            bundle_cache: Mutex::new(LruCache::new(config.storage_config.lru_capacity)),
            reaper_cache: Arc::new(Mutex::new(BTreeSet::new())),
            reaper_wakeup: Arc::new(tokio::sync::Notify::new()),
            max_cached_bundle_size: config.storage_config.max_cached_bundle_size.into(),
            reaper_cache_size: config.poll_channel_depth.into(),
        }
    }

    pub fn start(self: &Arc<Self>, dispatcher: Arc<dispatcher::Dispatcher>, recover_storage: bool) {
        if recover_storage {
            self.recover(&dispatcher);
        }

        // Start the reaper
        let store = self.clone();
        let task = async move { store.run_reaper(dispatcher).await };

        #[cfg(feature = "tracing")]
        let task = {
            let span = tracing::trace_span!(parent: None, "reaper_task");
            span.follows_from(tracing::Span::current());
            task.instrument(span)
        };

        self.task_tracker.spawn(task);
    }

    pub async fn shutdown(&self) {
        self.cancel_token.cancel();
        self.task_tracker.close();
        self.task_tracker.wait().await;
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all,fields(bundle.id = %bundle.id)))]
    pub async fn store(
        &self,
        bundle: hardy_bpv7::bundle::Bundle,
        data: Bytes,
    ) -> Option<bundle::Bundle> {
        // Write to bundle storage
        let storage_name = self.save_data(data).await;

        // Compose metadata
        let bundle = bundle::Bundle {
            metadata: metadata::BundleMetadata {
                storage_name: Some(storage_name.clone()),
                ..Default::default()
            },
            bundle,
        };

        // Write to metadata store
        match self.metadata_storage.insert(&bundle).await {
            Ok(true) => Some(bundle),
            Ok(false) => {
                // We have a duplicate, remove the duplicate from the bundle store
                if let Some(storage_name) = &bundle.metadata.storage_name {
                    self.delete_data(storage_name).await;
                }
                None
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

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    pub async fn load_data(&self, storage_name: &str) -> Option<Bytes> {
        if let Some(data) = self
            .bundle_cache
            .lock()
            .trace_expect("LRU cache lock error")
            .peek(storage_name)
        {
            return Some(data.clone());
        }

        self.bundle_storage
            .load(storage_name)
            .await
            .trace_expect("Failed to load bundle data")
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    pub async fn save_data(&self, data: Bytes) -> Arc<str> {
        let storage_name = self
            .bundle_storage
            .save(data.clone())
            .await
            .trace_expect("Failed to save bundle data");

        if data.len() < self.max_cached_bundle_size {
            self.bundle_cache
                .lock()
                .trace_expect("LRU cache lock error")
                .put(storage_name.clone(), data);
        }

        storage_name
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    pub async fn delete_data(&self, storage_name: &str) {
        self.bundle_cache
            .lock()
            .expect("LRU cache lock failure")
            .pop(storage_name);

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
                "Metadata store failed to return correct bundle: {:?} != {:?}",
                m.bundle.id, bundle_id
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
