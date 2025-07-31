use super::*;
use metadata::*;
use sha2::Digest;

const LRU_CAPACITY: usize = 256;

pub fn hash(data: &[u8]) -> Arc<[u8]> {
    sha2::Sha256::digest(data).to_vec().into()
}

pub struct Store {
    metadata_storage: Arc<dyn storage::MetadataStorage>,
    metadata_cache: std::sync::Mutex<lru::LruCache<hardy_bpv7::bundle::Id, ()>>,
    bundle_storage: Arc<dyn storage::BundleStorage>,
    bundle_cache: std::sync::Mutex<lru::LruCache<Arc<[u8]>, ()>>,
}

impl Store {
    pub fn new(config: &config::Config) -> Self {
        // Init pluggable storage engines
        Self {
            metadata_storage: config
                .metadata_storage
                .as_ref()
                .map(|s| s.clone())
                .unwrap_or(Arc::new(metadata_mem::Storage::default())),
            metadata_cache: std::sync::Mutex::new(lru::LruCache::new(
                std::num::NonZero::new(LRU_CAPACITY).unwrap(),
            )),
            bundle_storage: config
                .bundle_storage
                .as_ref()
                .map(|s| s.clone())
                .unwrap_or(Arc::new(bundle_mem::Storage::default())),
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
    ) {
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

        self.metadata_storage
            .remove_unconfirmed_bundles(tx)
            .await
            .trace_expect("Failed to get unconfirmed bundles");

        let bundles = h.await.trace_expect("Task terminated unexpectedly");
        info!("Metadata storage check complete, {bundles} bundles cleaned up");
    }

    #[instrument(skip_all)]
    async fn list_stored_bundles(
        &self,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Vec<storage::ListResponse> {
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

        self.bundle_storage
            .list(tx)
            .await
            .trace_expect("Failed to list stored bundles");

        h.await.trace_expect("Task terminated unexpectedly")
    }

    #[instrument(skip_all)]
    pub async fn bundle_storage_check(
        self: &Arc<Self>,
        dispatcher: Arc<dispatcher::Dispatcher>,
        cancel_token: &tokio_util::sync::CancellationToken,
    ) {
        // We're going to spawn a bunch of tasks
        let mut task_set = tokio::task::JoinSet::new();

        // Give some feedback
        let mut timer = tokio::time::interval(tokio::time::Duration::from_secs(5));
        let mut bundles = 0u64;
        let mut orphans = 0u64;
        let mut bad = 0u64;

        // For each bundle in the store
        for (storage_name, file_time) in self.list_stored_bundles(cancel_token.clone()).await {
            bundles = bundles.saturating_add(1);

            loop {
                tokio::select! {
                    biased;
                    _ = cancel_token.cancelled() => break,
                    _ = timer.tick() => {
                        info!("Bundle store restart in progress, {bundles} bundles processed, {orphans} orphan and {bad} bad bundles found");
                    },
                    Some(r) = task_set.join_next(), if !task_set.is_empty() => {
                        let (o,b) = r.trace_expect("Task terminated unexpectedly");
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
            let (o, b) = r.trace_expect("Task terminated unexpectedly");
            orphans = orphans.saturating_add(o);
            bad = bad.saturating_add(b);
        }
        info!(
            "Bundle store restart complete: {bundles} bundles processed, {orphans} orphan and {bad} bad bundles found"
        );
    }

    #[inline]
    pub async fn load_data(&self, storage_name: &str) -> storage::Result<Option<Bytes>> {
        self.bundle_storage.load(storage_name).await
    }

    pub async fn store_data(
        &self,
        data: Bytes,
        hash: Arc<[u8]>,
    ) -> storage::Result<Option<Arc<str>>> {
        if self
            .bundle_cache
            .lock()
            .trace_expect("LRU cache lock error")
            .put(hash, ())
            .is_some()
        {
            return Ok(None);
        }

        // Write to bundle storage
        self.bundle_storage.store(data).await.map(Some)
    }

    #[inline]
    pub async fn store_metadata(&self, bundle: &bundle::Bundle) -> storage::Result<bool> {
        if self
            .metadata_cache
            .lock()
            .trace_expect("LRU cache lock error")
            .put(bundle.bundle.id.clone(), ())
            .is_some()
        {
            return Ok(false);
        }

        self.metadata_storage.store(bundle).await
    }

    #[inline]
    pub async fn load(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> storage::Result<Option<bundle::Bundle>> {
        self.metadata_storage.load(bundle_id).await
    }

    #[instrument(skip(self, data))]
    pub async fn store(
        &self,
        bundle: hardy_bpv7::bundle::Bundle,
        data: Bytes,
        received_at: Option<time::OffsetDateTime>,
    ) -> storage::Result<Option<bundle::Bundle>> {
        // Write to bundle storage
        let hash = store::hash(&data);
        let Some(storage_name) = self.store_data(data, hash.clone()).await? else {
            return Ok(None);
        };

        // Compose metadata
        let bundle = bundle::Bundle {
            metadata: BundleMetadata {
                storage_name: Some(storage_name.clone()),
                hash: Some(hash),
                received_at,
            },
            bundle,
        };

        // Write to metadata store
        match self.store_metadata(&bundle).await {
            Ok(true) => Ok(Some(bundle)),
            Ok(false) => {
                // We have a duplicate, remove the duplicate from the bundle store
                self.remove_data(&bundle.metadata).await.map(|_| None)
            }
            Err(e) => {
                // This is just bad, we can't really claim to have stored the bundle,
                // so just cleanup and get out
                self.remove_data(&bundle.metadata).await.and(Err(e))
            }
        }
    }

    pub async fn remove_data(&self, metadata: &BundleMetadata) -> storage::Result<()> {
        if let Some(hash) = &metadata.hash {
            self.bundle_cache
                .lock()
                .expect("LRU cache lock failure")
                .pop(hash);
        }

        if let Some(storage_name) = &metadata.storage_name {
            // Delete the bundle from the bundle store
            self.bundle_storage.remove(storage_name).await
        } else {
            Ok(())
        }
    }

    pub async fn remove_metadata(&self, bundle_id: &hardy_bpv7::bundle::Id) -> storage::Result<()> {
        self.metadata_cache
            .lock()
            .expect("LRU cache lock failure")
            .pop(bundle_id);

        self.metadata_storage.remove(bundle_id).await
    }

    pub async fn confirm_exists(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> storage::Result<Option<metadata::BundleMetadata>> {
        self.metadata_storage.confirm_exists(bundle_id).await
    }
}
