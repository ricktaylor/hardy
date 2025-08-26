use super::*;
use lru::LruCache;
use std::sync::Mutex;

const LRU_CAPACITY: usize = 1024;
const MAX_CACHED_BUNDLE_SIZE: usize = 16 * 1024;

pub(crate) enum RestartResult {
    Missing,
    Duplicate,
    Restarted,
    Orphan,
    Junk,
}

struct RestartStats {
    lost: u64,
    duplicates: u64,
    restarted: u64,
    orphans: u64,
    junk: u64,
}

impl RestartStats {
    fn new() -> Self {
        Self {
            lost: 0,
            duplicates: 0,
            restarted: 0,
            orphans: 0,
            junk: 0,
        }
    }

    fn add(&mut self, r: RestartResult) {
        match r {
            RestartResult::Missing => self.lost = self.lost.saturating_add(1),
            RestartResult::Duplicate => self.duplicates = self.duplicates.saturating_add(1),
            RestartResult::Restarted => self.restarted = self.restarted.saturating_add(1),
            RestartResult::Orphan => self.orphans = self.orphans.saturating_add(1),
            RestartResult::Junk => self.junk = self.junk.saturating_add(1),
        }
    }

    fn trace(&self) {
        tracing::event!(
            target: "metrics",
            tracing::Level::TRACE,
            monotonic_counter.bpa.store.restart.lost_bundles = self.lost,
            monotonic_counter.bpa.store.restart.duplicate_bundles = self.duplicates,
            monotonic_counter.bpa.store.restart.restarted_bundles = self.restarted,
            monotonic_counter.bpa.store.restart.orphan_bundles = self.orphans,
            monotonic_counter.bpa.store.restart.junk_bundles = self.junk,
        );
    }
}

impl core::fmt::Display for RestartStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} bundles restarted, {} orphan and {} bad bundles found",
            self.restarted,
            self.orphans,
            self.lost + self.junk + self.duplicates
        )
    }
}

pub struct Store {
    cancel_token: tokio_util::sync::CancellationToken,
    task_tracker: tokio_util::task::TaskTracker,
    metadata_storage: Arc<dyn storage::MetadataStorage>,
    metadata_cache: Mutex<LruCache<hardy_bpv7::bundle::Id, Option<bundle::Bundle>>>,
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
            metadata_cache: Mutex::new(LruCache::new(
                std::num::NonZero::new(LRU_CAPACITY).unwrap(),
            )),
            bundle_storage: config
                .bundle_storage
                .as_ref()
                .map(|s| s.clone())
                .unwrap_or(bundle_mem::new(&bundle_mem::Config::default())),
            bundle_cache: Mutex::new(LruCache::new(std::num::NonZero::new(LRU_CAPACITY).unwrap())),
        }
    }

    pub fn start(self: &Arc<Self>, dispatcher: Arc<dispatcher::Dispatcher>) {
        // Start the store - this can take a while as the store is walked
        let store = self.clone();
        let span = tracing::trace_span!("parent: None", "store_check_task");
        span.follows_from(tracing::Span::current());
        self.task_tracker.spawn(
            async move {
                // Start the store - this can take a while as the store is walked
                info!("Starting store consistency check...");

                let stats = store
                    .bundle_storage_check(dispatcher.clone())
                    .await
                    .trace_expect("Bundle storage check failed");
                let stats = if !store.cancel_token.is_cancelled() {
                    store
                        .metadata_storage_check(dispatcher, stats)
                        .await
                        .trace_expect("Metadata storage check failed")
                } else {
                    stats
                };

                if !store.cancel_token.is_cancelled() {
                    info!("Store restarted: {stats}");
                }
            }
            .instrument(span),
        );
    }

    pub async fn shutdown(&self) {
        self.cancel_token.cancel();
        self.task_tracker.close();
        self.task_tracker.wait().await;
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    async fn bundle_storage_check(
        self: &Arc<Self>,
        dispatcher: Arc<dispatcher::Dispatcher>,
    ) -> storage::Result<RestartStats> {
        let outer_cancel_token = self.cancel_token.child_token();
        let cancel_token = outer_cancel_token.clone();
        let span = tracing::trace_span!("parent: None", "bundle_storage_check_reader");
        span.follows_from(tracing::Span::current());
        let (tx, rx) = flume::bounded::<storage::ListResponse>(16);
        let h = tokio::spawn(
            async move {
                // We're going to spawn a bunch of tasks
                let mut task_set = tokio::task::JoinSet::new();

                // Give some feedback
                let mut timer = tokio::time::interval(tokio::time::Duration::from_secs(1));
                let mut stats = RestartStats::new();

                loop {
                    tokio::select! {
                        _ = timer.tick() => {
                            stats.trace();
                        },
                        r = rx.recv_async() => match r {
                            Err(_) => {
                                break;
                            }
                            Ok(r) => {
                                // TODO: Use a semaphore to rate control this

                                //let dispatcher = dispatcher.clone();
                                //task_set.spawn(async move {
                                stats.add(dispatcher.restart_bundle(r.0,r.1).await?);
                                //});
                            }
                        },
                        Some(r) = task_set.join_next(), if !task_set.is_empty() => {
                            stats.add(r?);
                        },
                        _ = cancel_token.cancelled() => {
                            break;
                        }
                    }
                }

                // Wait for all sub-tasks to complete
                while let Some(r) = task_set.join_next().await {
                    stats.add(r?);
                }

                stats.trace();
                Ok(stats)
            }
            .instrument(span),
        );

        if let Err(e) = self.bundle_storage.list(tx).await {
            outer_cancel_token.cancel();
            _ = h.await;
            Err(e)
        } else {
            h.await?
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    async fn metadata_storage_check(
        self: &Arc<Self>,
        dispatcher: Arc<dispatcher::Dispatcher>,
        mut stats: RestartStats,
    ) -> storage::Result<RestartStats> {
        let outer_cancel_token = self.cancel_token.child_token();
        let cancel_token = outer_cancel_token.clone();
        let span = tracing::trace_span!("parent: None", "metadata_storage_check_reader");
        span.follows_from(tracing::Span::current());
        let (tx, rx) = flume::bounded::<bundle::Bundle>(16);
        let h = tokio::spawn(
            async move {
                // Give some feedback
                let mut timer = tokio::time::interval(tokio::time::Duration::from_secs(1));

                loop {
                    tokio::select! {
                        _ = timer.tick() => {
                            stats.trace();
                        },
                        bundle = rx.recv_async() => match bundle {
                            Err(_) => break,
                            Ok(bundle) => {
                                stats.add(RestartResult::Orphan);

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

                stats.trace();
                Ok(stats)
            }
            .instrument(span),
        );

        if let Err(e) = self.metadata_storage.remove_unconfirmed(tx).await {
            outer_cancel_token.cancel();
            _ = h.await;
            Err(e)
        } else {
            h.await?
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
        if let Some(bundle) = self
            .metadata_cache
            .lock()
            .trace_expect("LRU cache lock error")
            .get(bundle_id)
        {
            return Ok(bundle.clone());
        }

        self.metadata_storage.get(bundle_id).await
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    pub async fn insert_metadata(&self, bundle: &bundle::Bundle) -> storage::Result<bool> {
        // Check cache first
        if self
            .metadata_cache
            .lock()
            .trace_expect("LRU cache lock error")
            .contains(&bundle.bundle.id)
        {
            return Ok(false);
        }

        let not_found = self.metadata_storage.insert(bundle).await?;

        self.metadata_cache
            .lock()
            .trace_expect("LRU cache lock error")
            .put(bundle.bundle.id.clone(), not_found.then(|| bundle.clone()));

        Ok(not_found)
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    pub async fn tombstone_metadata(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> storage::Result<()> {
        self.metadata_cache
            .lock()
            .trace_expect("LRU cache lock error")
            .put(bundle_id.clone(), None);

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
        self.metadata_cache
            .lock()
            .trace_expect("LRU cache lock error")
            .put(bundle.bundle.id.clone(), Some(bundle.clone()));

        self.metadata_storage.replace(bundle).await
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    pub async fn poll_pending(&self, tx: storage::Sender<bundle::Bundle>) -> storage::Result<()> {
        self.metadata_storage.poll_pending(tx).await
    }
}
