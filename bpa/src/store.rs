use super::*;
use lru::LruCache;
use std::sync::Mutex;

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

pub struct Store {
    cancel_token: tokio_util::sync::CancellationToken,
    task_tracker: tokio_util::task::TaskTracker,
    metadata_storage: Arc<dyn storage::MetadataStorage>,
    bundle_storage: Arc<dyn storage::BundleStorage>,
    bundle_cache: Mutex<LruCache<Arc<str>, Bytes>>,
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
        }
    }

    pub fn start(self: &Arc<Self>, dispatcher: Arc<dispatcher::Dispatcher>, recover_storage: bool) {
        if recover_storage {
            // Start the store - this can take a while as the store is walked
            let store = self.clone();
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

                store
                    .bundle_storage_recovery(dispatcher.clone())
                    .await
                    .trace_expect("Bundle storage check failed");

                if !store.cancel_token.is_cancelled() {
                    store
                        .metadata_storage_recovery(dispatcher)
                        .await
                        .trace_expect("Metadata storage check failed")
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
    async fn bundle_storage_recovery(
        self: &Arc<Self>,
        dispatcher: Arc<dispatcher::Dispatcher>,
    ) -> storage::Result<()> {
        let outer_cancel_token = self.cancel_token.child_token();
        let cancel_token = outer_cancel_token.clone();
        let (tx, rx) = flume::bounded::<storage::RecoveryResponse>(16);
        let task = async move {
            // We're going to spawn a bunch of tasks
            let parallelism = std::thread::available_parallelism()
                .map(Into::into)
                .unwrap_or(1);
            let mut task_set = tokio::task::JoinSet::new();
            let semaphore = Arc::new(tokio::sync::Semaphore::new(parallelism));

            loop {
                tokio::select! {
                    r = rx.recv_async() => match r {
                        Err(_) => {
                            break;
                        }
                        Ok(r) => {
                            let permit = semaphore.clone().acquire_owned().await.trace_expect("Failed to acquire permit");
                            let dispatcher = dispatcher.clone();
                            task_set.spawn(async move {
                                match dispatcher.restart_bundle(r.0,r.1).await {
                                    Ok(RestartResult::Missing) => metrics::counter!("restart_lost_bundles").increment(1),
                                    Ok(RestartResult::Duplicate) => metrics::counter!("restart_duplicate_bundles").increment(1),
                                    Ok(RestartResult::Valid) => metrics::counter!("restart_valid_bundles").increment(1),
                                    Ok(RestartResult::Orphan) => metrics::counter!("restart_orphan_bundles").increment(1),
                                    Ok(RestartResult::Junk) => metrics::counter!("restart_junk_bundles").increment(1),
                                    Err(e) => error!("Failed to restart bundle: {e}")
                                }
                                drop(permit);
                            });
                        }
                    },
                    Some(_) = task_set.join_next(), if !task_set.is_empty() => {},
                    _ = cancel_token.cancelled() => {
                        break;
                    }
                }
            }

            // Wait for all sub-tasks to complete
            while task_set.join_next().await.is_some() {}
        };

        #[cfg(feature = "tracing")]
        let task = {
            let span = tracing::trace_span!("parent: None", "bundle_storage_recovery_reader");
            span.follows_from(tracing::Span::current());
            task.instrument(span)
        };

        let h = tokio::spawn(task);

        if let Err(e) = self.bundle_storage.recover(tx).await {
            outer_cancel_token.cancel();
            _ = h.await;
            Err(e)
        } else {
            _ = h.await;
            Ok(())
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    async fn metadata_storage_recovery(
        self: &Arc<Self>,
        dispatcher: Arc<dispatcher::Dispatcher>,
    ) -> storage::Result<()> {
        let outer_cancel_token = self.cancel_token.child_token();
        let cancel_token = outer_cancel_token.clone();
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

        if let Err(e) = self.metadata_storage.remove_unconfirmed(tx).await {
            outer_cancel_token.cancel();
            _ = h.await;
            Err(e)
        } else {
            _ = h.await;
            Ok(())
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    pub async fn store(
        &self,
        bundle: hardy_bpv7::bundle::Bundle,
        data: Bytes,
    ) -> storage::Result<Option<bundle::Bundle>> {
        // Write to bundle storage
        let storage_name = self.save_data(data).await?;

        // Compose metadata
        let bundle = bundle::Bundle {
            metadata: metadata::BundleMetadata {
                status: metadata::BundleStatus::Dispatching,
                storage_name: Some(storage_name.clone()),
                received_at: time::OffsetDateTime::now_utc(),
                non_canonical: false,
            },
            bundle,
        };

        // Write to metadata store
        match self.insert_metadata(&bundle).await {
            Ok(true) => Ok(Some(bundle)),
            Ok(false) => {
                // We have a duplicate, remove the duplicate from the bundle store
                if let Some(storage_name) = &bundle.metadata.storage_name {
                    self.delete_data(storage_name).await?;
                }
                Ok(None)
            }
            Err(e) => {
                // This is just bad, we can't really claim to have stored the bundle,
                // so just cleanup and get out
                if let Some(storage_name) = &bundle.metadata.storage_name {
                    _ = self.delete_data(storage_name).await;
                }
                Err(e)
            }
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    pub async fn load_data(&self, storage_name: &str) -> storage::Result<Option<Bytes>> {
        if let Some(data) = self
            .bundle_cache
            .lock()
            .trace_expect("LRU cache lock error")
            .peek(storage_name)
        {
            return Ok(Some(data.clone()));
        }

        self.bundle_storage.load(storage_name).await
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    pub async fn save_data(&self, data: Bytes) -> storage::Result<Arc<str>> {
        if data.len() < MAX_CACHED_BUNDLE_SIZE {
            let storage_name = self.bundle_storage.save(data.clone()).await?;

            self.bundle_cache
                .lock()
                .trace_expect("LRU cache lock error")
                .put(storage_name.clone(), data);

            Ok(storage_name)
        } else {
            self.bundle_storage.save(data).await
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    pub async fn delete_data(&self, storage_name: &str) -> storage::Result<()> {
        self.bundle_cache
            .lock()
            .expect("LRU cache lock failure")
            .pop(storage_name);

        self.bundle_storage.delete(storage_name).await
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    pub async fn get_metadata(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> storage::Result<Option<bundle::Bundle>> {
        self.metadata_storage.get(bundle_id).await
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    pub async fn insert_metadata(&self, bundle: &bundle::Bundle) -> storage::Result<bool> {
        self.metadata_storage.insert(bundle).await
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    pub async fn tombstone_metadata(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> storage::Result<()> {
        self.metadata_storage.tombstone(bundle_id).await
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    pub async fn confirm_exists(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> storage::Result<Option<metadata::BundleMetadata>> {
        self.metadata_storage.confirm_exists(bundle_id).await
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    pub async fn update_metadata(&self, bundle: &bundle::Bundle) -> storage::Result<()> {
        self.metadata_storage.replace(bundle).await
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    pub async fn poll_expiry(
        &self,
        tx: storage::Sender<bundle::Bundle>,
        limit: usize,
    ) -> storage::Result<()> {
        self.metadata_storage.poll_expiry(tx, limit).await
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    pub async fn poll_waiting(&self, tx: storage::Sender<bundle::Bundle>) -> storage::Result<()> {
        self.metadata_storage.poll_waiting(tx).await
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    pub async fn reset_peer_queue(&self, peer: u32) -> storage::Result<bool> {
        self.metadata_storage.reset_peer_queue(peer).await
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    pub async fn poll_pending(
        &self,
        tx: storage::Sender<bundle::Bundle>,
        state: &metadata::BundleStatus,
        limit: usize,
    ) -> storage::Result<()> {
        self.metadata_storage.poll_pending(tx, state, limit).await
    }
}
