use super::*;

const LRU_CAPACITY: usize = 256;
const MAX_CACHED_BUNDLE_SIZE: usize = 4096;

pub struct Store {
    metadata_storage: Arc<dyn storage::MetadataStorage>,
    metadata_cache: std::sync::Mutex<lru::LruCache<hardy_bpv7::bundle::Id, Option<bundle::Bundle>>>,
    bundle_storage: Arc<dyn storage::BundleStorage>,
    bundle_cache: std::sync::Mutex<lru::LruCache<Arc<str>, Bytes>>,
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
            metadata_cache: std::sync::Mutex::new(lru::LruCache::new(
                std::num::NonZero::new(LRU_CAPACITY).unwrap(),
            )),
            bundle_storage: config
                .bundle_storage
                .as_ref()
                .map(|s| s.clone())
                .unwrap_or(bundle_mem::new(&bundle_mem::Config::default())),
            bundle_cache: std::sync::Mutex::new(lru::LruCache::new(
                std::num::NonZero::new(LRU_CAPACITY).unwrap(),
            )),
        }
    }

    #[instrument(skip_all)]
    pub async fn metadata_storage_check(
        &self,
        dispatcher: Arc<dispatcher::Dispatcher>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<(), storage::Error> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<bundle::Bundle>(16);
        let h = tokio::spawn(async move {
            // Give some feedback
            let mut bundles = 0u64;
            let mut timer = tokio::time::interval(tokio::time::Duration::from_secs(5));

            loop {
                tokio::select! {
                    _ = timer.tick() => {
                        info!("Metadata storage check in progress, {bundles} bundles cleaned up");
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
            bundles
        });

        self.metadata_storage.remove_unconfirmed(tx).await?;

        let bundles = h.await?;
        info!("Metadata storage check complete, {bundles} bundles cleaned up");

        Ok(())
    }

    #[instrument(skip_all)]
    async fn list_stored_bundles(
        &self,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<Vec<storage::ListResponse>, storage::Error> {
        /* This is done as a big Vec buffer, as we cannot start processing stored bundles
         * until we have enumerated them all, as the processing can create more report bundles
         * which causes all kinds of double-processing issues */

        // TODO: We might want to use a tempfile here as the Vec<> could get really big!

        const CHUNK_SIZE: usize = 128;

        let (tx, mut rx) = tokio::sync::mpsc::channel::<storage::ListResponse>(CHUNK_SIZE);
        let h = tokio::spawn(async move {
            let mut results = Vec::new();

            // Give some feedback
            let mut bundles = 0u64;
            let mut timer = tokio::time::interval(tokio::time::Duration::from_secs(5));

            loop {
                tokio::select! {
                    _ = timer.tick() => {
                        info!("Bundle storage restart in progress, {bundles} bundles found");
                    },
                    r = rx.recv_many(&mut results,CHUNK_SIZE) => {
                        if r == 0 {
                            break;
                        } else {
                            bundles = bundles.saturating_add(r as u64);
                        }
                    },
                    _ = cancel_token.cancelled() => {
                        rx.close()
                    }
                }
            }
            results
        });

        self.bundle_storage.list(tx).await?;

        h.await.map_err(Into::into)
    }

    #[instrument(skip_all)]
    pub async fn bundle_storage_check(
        self: &Arc<Self>,
        dispatcher: Arc<dispatcher::Dispatcher>,
        cancel_token: &tokio_util::sync::CancellationToken,
    ) -> Result<(), storage::Error> {
        // We're going to spawn a bunch of tasks
        let mut task_set = tokio::task::JoinSet::new();

        // Give some feedback
        let mut timer = tokio::time::interval(tokio::time::Duration::from_secs(5));
        let mut bundles = 0u64;
        let mut orphans = 0u64;
        let mut bad = 0u64;

        // For each bundle in the store
        for (storage_name, file_time) in self.list_stored_bundles(cancel_token.clone()).await? {
            bundles = bundles.saturating_add(1);

            loop {
                tokio::select! {
                    biased;
                    _ = cancel_token.cancelled() => break,
                    _ = timer.tick() => {
                        info!("Bundle store restart in progress, {bundles} bundles processed, {orphans} orphan and {bad} bad bundles found");
                    },
                    Some(r) = task_set.join_next(), if !task_set.is_empty() => {
                        let (o,b) = r??;
                        orphans = orphans.saturating_add(o);
                        bad = bad.saturating_add(b);
                    },
                    _ = std::future::ready(()) => {
                        let dispatcher = dispatcher.clone();
                        let storage_name = storage_name.clone();
                        task_set.spawn(async move {
                            dispatcher.restart_bundle(storage_name, file_time).await
                        });
                    }
                }
            }
        }

        // Wait for all sub-tasks to complete
        while let Some(r) = task_set.join_next().await {
            let (o, b) = r??;
            orphans = orphans.saturating_add(o);
            bad = bad.saturating_add(b);
        }
        info!(
            "Bundle store restart complete: {bundles} bundles processed, {orphans} orphan and {bad} bad bundles found"
        );
        Ok(())
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

    pub async fn load_data(&self, storage_name: &str) -> storage::Result<Option<Bytes>> {
        if let Some(data) = self
            .bundle_cache
            .lock()
            .trace_expect("LRU cache lock error")
            .get(storage_name)
        {
            return Ok(Some(data.clone()));
        }

        self.bundle_storage.load(storage_name).await
    }

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

    pub async fn delete_data(&self, storage_name: &str) -> storage::Result<()> {
        self.bundle_cache
            .lock()
            .expect("LRU cache lock failure")
            .pop(storage_name);

        self.bundle_storage.delete(storage_name).await
    }

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

    pub async fn insert_metadata(&self, bundle: &bundle::Bundle) -> storage::Result<bool> {
        let found = self.metadata_storage.insert(bundle).await?;

        self.metadata_cache
            .lock()
            .trace_expect("LRU cache lock error")
            .put(bundle.bundle.id.clone(), found.then(|| bundle.clone()));

        Ok(found)
    }

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

    pub async fn confirm_exists(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> storage::Result<Option<metadata::BundleMetadata>> {
        self.metadata_storage.confirm_exists(bundle_id).await
    }

    pub async fn update_metadata(&self, bundle: &bundle::Bundle) -> storage::Result<()> {
        self.metadata_cache
            .lock()
            .trace_expect("LRU cache lock error")
            .put(bundle.bundle.id.clone(), Some(bundle.clone()));

        self.metadata_storage.replace(bundle).await
    }
}
