use super::*;
use lru::LruCache;
use std::sync::Mutex;

const LRU_CAPACITY: usize = 1024;
const MAX_CACHED_BUNDLE_SIZE: usize = 16 * 1024;

pub struct Store {
    metadata_storage: Arc<dyn storage::MetadataStorage>,
    metadata_cache: Mutex<LruCache<hardy_bpv7::bundle::Id, Option<bundle::Bundle>>>,
    bundle_storage: Arc<dyn storage::BundleStorage>,
    bundle_cache: Mutex<LruCache<Arc<str>, Bytes>>,
}

impl Store {
    pub fn new(config: &config::Config) -> Self {
        // Init pluggable storage engines
        Self {
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

    #[instrument(skip_all)]
    pub async fn start(
        &self,
        dispatcher: Arc<dispatcher::Dispatcher>,
        cancel_token: &tokio_util::sync::CancellationToken,
    ) -> Result<(), storage::Error> {
        // Start the store - this can take a while as the store is walked
        info!("Starting store consistency check...");

        if self
            .bundle_storage_check(dispatcher.clone(), cancel_token)
            .await?
            && self
                .metadata_storage_check(dispatcher, cancel_token)
                .await?
        {
            info!("Store restarted");
        }
        Ok(())
    }

    #[instrument(skip_all)]
    async fn bundle_storage_check(
        &self,
        dispatcher: Arc<dispatcher::Dispatcher>,
        cancel_token: &tokio_util::sync::CancellationToken,
    ) -> Result<bool, storage::Error> {
        let outer_cancel_token = cancel_token.child_token();
        let cancel_token = outer_cancel_token.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<storage::ListResponse>(16);
        let h = tokio::spawn(async move {
            // We're going to spawn a bunch of tasks
            let mut task_set = tokio::task::JoinSet::new();

            // Give some feedback
            let mut timer = tokio::time::interval(tokio::time::Duration::from_secs(5));
            let mut bundles = 0u64;
            let mut orphans = 0u64;
            let mut bad = 0u64;

            loop {
                tokio::select! {
                    _ = timer.tick() => {
                        info!("Bundle store restart in progress, {bundles} bundles processed, {orphans} orphan and {bad} bad bundles found");
                    },
                    r = rx.recv() => match r {
                        None => {
                            break;
                        }
                        Some(r) => {
                            bundles = bundles.saturating_add(1);
                            let dispatcher = dispatcher.clone();
                            task_set.spawn(async move {
                                dispatcher.restart_bundle(r.0,r.1).await
                            });
                        }
                    },
                    Some(r) = task_set.join_next(), if !task_set.is_empty() => {
                        let (o,b) = r??;
                        orphans = orphans.saturating_add(o);
                        bad = bad.saturating_add(b);
                    },
                    _ = cancel_token.cancelled() => {
                        rx.close()
                    }
                }
            }

            // Wait for all sub-tasks to complete
            while let Some(r) = task_set.join_next().await {
                let (o, b) = r??;
                orphans = orphans.saturating_add(o);
                bad = bad.saturating_add(b);
            }

            if !cancel_token.is_cancelled() {
                info!(
                    "Bundle store restart complete: {bundles} bundles processed, {orphans} orphan and {bad} bad bundles found"
                );
            }
            Ok(!cancel_token.is_cancelled())
        });

        if let Err(e) = self.bundle_storage.list(tx).await {
            outer_cancel_token.cancel();
            _ = h.await;
            Err(e)
        } else {
            h.await?
        }
    }

    #[instrument(skip_all)]
    async fn metadata_storage_check(
        &self,
        dispatcher: Arc<dispatcher::Dispatcher>,
        cancel_token: &tokio_util::sync::CancellationToken,
    ) -> Result<bool, storage::Error> {
        let outer_cancel_token = cancel_token.child_token();
        let cancel_token = outer_cancel_token.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<bundle::Bundle>(16);
        let h = tokio::spawn(async move {
            // Give some feedback
            let mut bundles = 0u64;
            let mut timer = tokio::time::interval(tokio::time::Duration::from_secs(5));

            loop {
                tokio::select! {
                    _ = timer.tick() => {
                        info!("Metadata storage check in progress, {bundles} expired bundles cleaned up");
                    },
                    bundle = rx.recv() => match bundle {
                        None => break,
                        Some(bundle) => {
                            bundles = bundles.saturating_add(1);

                            // The data associated with `bundle` has gone!
                            dispatcher.report_bundle_deletion(
                                &bundle,
                                hardy_bpv7::status_report::ReasonCode::DepletedStorage,
                            )
                            .await
                        }
                    },
                    _ = cancel_token.cancelled() => {
                        rx.close();
                    }
                }
            }

            if !cancel_token.is_cancelled() {
                info!("Metadata storage check complete, {bundles} expired bundles cleaned up");
            }
            Ok(!cancel_token.is_cancelled())
        });

        if let Err(e) = self.metadata_storage.remove_unconfirmed(tx).await {
            outer_cancel_token.cancel();
            _ = h.await;
            Err(e)
        } else {
            h.await?
        }
    }

    #[instrument(skip(self, data))]
    pub async fn store(
        &self,
        bundle: hardy_bpv7::bundle::Bundle,
        data: Bytes,
        received_at: Option<time::OffsetDateTime>,
    ) -> storage::Result<Option<bundle::Bundle>> {
        // Write to bundle storage
        let storage_name = self.save_data(data).await?;

        // Compose metadata
        let bundle = bundle::Bundle {
            metadata: metadata::BundleMetadata {
                storage_name: Some(storage_name.clone()),
                received_at,
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

    #[instrument(skip(self))]
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

    #[instrument(skip(self,data))]
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

    #[instrument(skip(self))]
    pub async fn delete_data(&self, storage_name: &str) -> storage::Result<()> {
        self.bundle_cache
            .lock()
            .expect("LRU cache lock failure")
            .pop(storage_name);

        self.bundle_storage.delete(storage_name).await
    }

    #[instrument(skip(self))]
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

    #[instrument(skip(self))]
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

    #[instrument(skip(self))]
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

    #[instrument(skip(self))]
    pub async fn confirm_exists(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> storage::Result<Option<metadata::BundleMetadata>> {
        self.metadata_storage.confirm_exists(bundle_id).await
    }

    #[instrument(skip(self))]
    pub async fn update_metadata(&self, bundle: &bundle::Bundle) -> storage::Result<()> {
        self.metadata_cache
            .lock()
            .trace_expect("LRU cache lock error")
            .put(bundle.bundle.id.clone(), Some(bundle.clone()));

        self.metadata_storage.replace(bundle).await
    }
}
