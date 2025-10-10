use super::*;

// TODO: Make these config options
const LRU_CAPACITY: usize = 1024;
const MAX_CACHED_BUNDLE_SIZE: usize = 16 * 1024;

pub(crate) enum RestartResult {
    Missing,
    Duplicate,
    Valid,
    Orphan,
    Junk,
}

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
            bundle_cache: Mutex::new(LruCache::new(std::num::NonZero::new(LRU_CAPACITY).unwrap())),
            reaper_cache: Arc::new(Mutex::new(BTreeSet::new())),
            reaper_wakeup: Arc::new(tokio::sync::Notify::new()),
        }
    }

    pub fn start(self: &Arc<Self>, dispatcher: Arc<dispatcher::Dispatcher>, recover_storage: bool) {
        if recover_storage {
            // Start the store - this can take a while as the store is walked
            let store = self.clone();
            let dispatcher = dispatcher.clone();
            let task = async move {
                // Start the store - this can take a while as the store is walked
                info!("Starting store consistency check...");

                // Set up the metrics
                metrics::describe_counter!(
                    "restart_lost_bundles",
                    metrics::Unit::Count,
                    "Total number of lost bundles discovered during storage restart"
                );
                metrics::describe_counter!(
                    "restart_duplicate_bundles",
                    metrics::Unit::Count,
                    "Total number of duplicate bundles discovered during storage restart"
                );
                metrics::describe_counter!(
                    "restart_valid_bundles",
                    metrics::Unit::Count,
                    "Total number of valid bundles discovered during storage restart"
                );
                metrics::describe_counter!(
                    "restart_orphan_bundles",
                    metrics::Unit::Count,
                    "Total number of orphaned bundles discovered during storage restart"
                );
                metrics::describe_counter!(
                    "restart_junk_bundles",
                    metrics::Unit::Count,
                    "Total number of junk bundles discovered during storage restart"
                );

                store.start_metadata_storage_recovery().await;

                store.bundle_storage_recovery(dispatcher.clone()).await;

                if !store.cancel_token.is_cancelled() {
                    store.metadata_storage_recovery(dispatcher).await;
                }
            };

            #[cfg(feature = "tracing")]
            let task = {
                let span = tracing::trace_span!("parent: None", "store_check_task");
                span.follows_from(tracing::Span::current());
                task.instrument(span)
            };

            self.task_tracker.spawn(task);
        }

        // Start the reaper
        let store = self.clone();
        let task = async move { store.run_reaper(dispatcher).await };

        #[cfg(feature = "tracing")]
        let task = {
            let span = tracing::trace_span!("parent: None", "reaper_task");
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

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    async fn start_metadata_storage_recovery(&self) {
        self.metadata_storage.start_recovery().await;
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    async fn bundle_storage_recovery(self: &Arc<Self>, dispatcher: Arc<dispatcher::Dispatcher>) {
        let cancel_token = self.cancel_token.clone();
        let (tx, rx) = flume::bounded::<storage::RecoveryResponse>(16);
        let task = async move {
            loop {
                tokio::select! {
                    r = rx.recv_async() => match r {
                        Err(_) => {
                            break;
                        }
                        Ok(r) => {
                            match dispatcher.restart_bundle(r.0,r.1).await {
                                RestartResult::Missing => metrics::counter!("restart_lost_bundles").increment(1),
                                RestartResult::Duplicate => metrics::counter!("restart_duplicate_bundles").increment(1),
                                RestartResult::Valid => metrics::counter!("restart_valid_bundles").increment(1),
                                RestartResult::Orphan => metrics::counter!("restart_orphan_bundles").increment(1),
                                RestartResult::Junk => metrics::counter!("restart_junk_bundles").increment(1),
                            }
                        }
                    },
                    _ = cancel_token.cancelled() => {
                        break;
                    }
                }
            }
        };

        #[cfg(feature = "tracing")]
        let task = {
            let span = tracing::trace_span!("parent: None", "bundle_storage_recovery_reader");
            span.follows_from(tracing::Span::current());
            task.instrument(span)
        };

        let h = tokio::spawn(task);

        self.bundle_storage
            .recover(tx)
            .await
            .trace_expect("Bundle storage recover failed");

        _ = h.await;
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    async fn metadata_storage_recovery(self: &Arc<Self>, dispatcher: Arc<dispatcher::Dispatcher>) {
        let cancel_token = self.cancel_token.clone();
        let (tx, rx) = flume::bounded::<bundle::Bundle>(16);
        let task = async move {
            loop {
                tokio::select! {
                    bundle = rx.recv_async() => match bundle {
                        Err(_) => break,
                        Ok(bundle) => {
                            metrics::counter!("restart_orphan_bundles").increment(1);

                            // The data associated with `bundle` has gone!
                            dispatcher.report_bundle_deletion(
                                &bundle,
                                hardy_bpv7::status_report::ReasonCode::DepletedStorage,
                            )
                            .await
                        }
                    },
                    _ = cancel_token.cancelled() => {
                        break;
                    }
                }
            }
        };

        #[cfg(feature = "tracing")]
        let task = {
            let span = tracing::trace_span!("parent: None", "metadata_storage_check_reader");
            span.follows_from(tracing::Span::current());
            task.instrument(span)
        };

        let h = tokio::spawn(task);

        self.metadata_storage
            .remove_unconfirmed(tx)
            .await
            .trace_expect("Remove unconfirmed bundles failed");

        _ = h.await;
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
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

        if data.len() < MAX_CACHED_BUNDLE_SIZE {
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

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    pub async fn insert_metadata(&self, bundle: &bundle::Bundle) -> bool {
        self.metadata_storage
            .insert(bundle)
            .await
            .trace_expect("Failed to insert metadata")
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    pub async fn get_metadata(&self, bundle_id: &hardy_bpv7::bundle::Id) -> Option<bundle::Bundle> {
        self.metadata_storage
            .get(bundle_id)
            .await
            .trace_expect("Failed to get metadata")
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    pub async fn tombstone_metadata(&self, bundle_id: &hardy_bpv7::bundle::Id) {
        self.metadata_storage
            .tombstone(bundle_id)
            .await
            .trace_expect("Failed to tombstone metadata")
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    pub async fn confirm_exists(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> Option<metadata::BundleMetadata> {
        self.metadata_storage
            .confirm_exists(bundle_id)
            .await
            .trace_expect("Failed to confirm bundle existence")
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
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
