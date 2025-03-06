use super::*;
use metadata::*;
use sha2::Digest;

fn hash(data: &[u8]) -> Arc<[u8]> {
    sha2::Sha256::digest(data).to_vec().into()
}

pub struct Store {
    wait_sample_interval: time::Duration,
    metadata_storage: Arc<dyn storage::MetadataStorage>,
    bundle_storage: Arc<dyn storage::BundleStorage>,
}

impl Store {
    pub fn new(config: &bpa::Config) -> Self {
        // Init pluggable storage engines
        Self {
            wait_sample_interval: config.wait_sample_interval,
            metadata_storage: config
                .metadata_storage
                .as_ref()
                .map(|s| s.clone())
                .unwrap_or(Arc::new(metadata_mem::Storage::default())),
            bundle_storage: config
                .bundle_storage
                .as_ref()
                .map(|s| s.clone())
                .unwrap_or(Arc::new(bundle_mem::Storage::default())),
        }
    }

    #[instrument(skip_all)]
    pub async fn start(
        &self,
        dispatcher: Arc<dispatcher::Dispatcher>,
        task_set: &mut tokio::task::JoinSet<()>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        info!("Starting store consistency check...");
        self.bundle_storage_check(dispatcher.clone(), cancel_token.clone())
            .await;

        // Now check the metadata storage for old data
        self.metadata_storage_check(dispatcher.clone(), cancel_token.clone())
            .await;

        info!("Store restarted");

        if !cancel_token.is_cancelled() {
            // Spawn a waiter
            let metadata_storage = self.metadata_storage.clone();
            task_set.spawn(Self::check_waiting(
                self.wait_sample_interval,
                metadata_storage,
                dispatcher,
                cancel_token,
            ));
        }
    }

    #[instrument(skip_all)]
    async fn metadata_storage_check(
        &self,
        dispatcher: Arc<dispatcher::Dispatcher>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        let (tx, mut rx) =
            tokio::sync::mpsc::channel::<(metadata::BundleMetadata, bpv7::Bundle)>(16);
        let metadata_storage = self.metadata_storage.clone();
        let h = tokio::spawn(async move {
            // Give some feedback
            let mut bundles = 0u64;
            let timer = tokio::time::sleep(tokio::time::Duration::from_secs(5));
            tokio::pin!(timer);

            loop {
                tokio::select! {
                    () = &mut timer => {
                        info!("Metadata storage check in progress, {bundles} bundles cleaned up");
                        timer.as_mut().reset(tokio::time::Instant::now() + tokio::time::Duration::from_secs(5));
                    },
                    bundle = rx.recv() => match bundle {
                        None => break,
                        Some((m,b)) => {
                            if let BundleStatus::Tombstone(_) = &m.status {
                                // Ignore Tombstones
                            } else {
                                bundles = bundles.saturating_add(1);
                                let bundle_id = b.id.clone();

                                // The data associated with `bundle` has gone!
                                dispatcher.report_bundle_deletion(
                                    &bundle::Bundle{
                                        metadata: m,
                                        bundle: b,
                                    },
                                    bpv7::StatusReportReasonCode::DepletedStorage,
                                )
                                .await.trace_expect("Failed to report bundle deletion");

                                // Delete it
                                metadata_storage
                                    .remove(&bundle_id)
                                    .await.trace_expect("Failed to remove orphan bundle")
                            }
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
            .get_unconfirmed_bundles(tx)
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
         * until we have enumerated them all, as the processing can create more bundles
         * which causes all kinds of double-processing issues */

        // TODO: We might want to use a tempfile here as the Vec<> could get really big!

        let (tx, mut rx) = tokio::sync::mpsc::channel::<storage::ListResponse>(16);
        let h = tokio::spawn(async move {
            let mut results = Vec::new();

            // Give some feedback
            let mut bundles = 0u64;
            let timer = tokio::time::sleep(tokio::time::Duration::from_secs(5));
            tokio::pin!(timer);

            loop {
                tokio::select! {
                    () = &mut timer => {
                        info!("Bundle storage check in progress, {bundles} bundles found");
                        timer.as_mut().reset(tokio::time::Instant::now() + tokio::time::Duration::from_secs(5));
                    },
                    r = rx.recv() => match r {
                        None => break,
                        Some(r) => {
                            bundles = bundles.saturating_add(1);
                            results.push(r);
                        },
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
    async fn bundle_storage_check(
        &self,
        dispatcher: Arc<dispatcher::Dispatcher>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        // We're going to spawn a bunch of tasks
        let parallelism = std::thread::available_parallelism()
            .map(Into::into)
            .unwrap_or(1)
            + 1;
        let mut task_set = tokio::task::JoinSet::new();
        let semaphore = Arc::new(tokio::sync::Semaphore::new(parallelism));

        // Give some feedback
        let timer = tokio::time::sleep(tokio::time::Duration::from_secs(5));
        tokio::pin!(timer);
        let mut bundles = 0u64;
        let mut orphans = 0u64;
        let mut bad = 0u64;

        // For each bundle in the store
        for (storage_name, file_time) in self.list_stored_bundles(cancel_token.clone()).await {
            bundles = bundles.saturating_add(1);

            loop {
                tokio::select! {
                    () = &mut timer => {
                        info!("Bundle store restart in progress, {bundles} bundles processed, {orphans} orphan and {bad} bad bundles found");
                        timer.as_mut().reset(tokio::time::Instant::now() + tokio::time::Duration::from_secs(5));
                    },
                    // Throttle the number of tasks
                    permit = semaphore.clone().acquire_owned() => {
                        // We have a permit to process a bundle
                        let permit = permit.trace_expect("Failed to acquire permit");
                        let metadata_storage = self.metadata_storage.clone();
                        let bundle_storage = self.bundle_storage.clone();
                        let dispatcher = dispatcher.clone();

                        task_set.spawn(async move {
                            let (o,b) = Self::restart_bundle(metadata_storage, bundle_storage, dispatcher, storage_name, file_time).await;
                            drop(permit);
                            (o,b)
                        });
                        break;
                    }
                    Some(r) = task_set.join_next(), if !task_set.is_empty() => {
                        let (o,b) = r.trace_expect("Task terminated unexpectedly");
                        orphans = orphans.saturating_add(o);
                        bad = bad.saturating_add(b);
                    },
                    _ = cancel_token.cancelled() => break
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

    #[instrument(skip(metadata_storage, bundle_storage, dispatcher))]
    async fn restart_bundle(
        metadata_storage: Arc<dyn storage::MetadataStorage>,
        bundle_storage: Arc<dyn storage::BundleStorage>,
        dispatcher: Arc<dispatcher::Dispatcher>,
        mut storage_name: Arc<str>,
        file_time: Option<time::OffsetDateTime>,
    ) -> (u64, u64) {
        let Some(data) = bundle_storage
            .load(&storage_name)
            .await
            .trace_expect(&format!("Failed to load bundle data: {storage_name}"))
        else {
            // Data has gone while we were restarting
            return (0, 0);
        };

        // Parse the bundle
        let (bundle, reason, hash, report_unsupported) =
            match bpv7::ValidBundle::parse(data.as_ref().as_ref(), |_, _| Ok(None)) {
                Ok(bpv7::ValidBundle::Valid(bundle, report_unsupported)) => (
                    bundle,
                    None,
                    Some(hash(data.as_ref().as_ref())),
                    report_unsupported,
                ),
                Ok(bpv7::ValidBundle::Rewritten(bundle, data, report_unsupported)) => {
                    warn!("Bundle in non-canonical format found: {storage_name}");

                    // Rewrite the bundle
                    let new_storage_name = bundle_storage
                        .store(&data)
                        .await
                        .trace_expect("Failed to store rewritten canonical bundle");

                    bundle_storage
                        .remove(&storage_name)
                        .await
                        .trace_expect(&format!(
                            "Failed to remove duplicate bundle: {storage_name}"
                        ));

                    storage_name = new_storage_name;
                    (bundle, None, Some(hash(&data)), report_unsupported)
                }
                Ok(bpv7::ValidBundle::Invalid(bundle, reason, e)) => {
                    warn!("Invalid bundle found: {storage_name}, {e}");
                    (
                        bundle,
                        Some(reason),
                        Some(hash(data.as_ref().as_ref())),
                        false,
                    )
                }
                Err(e) => {
                    // Parse failed badly, no idea who to report to
                    warn!("Junk data found: {storage_name}, {e}");

                    // Drop the bundle
                    bundle_storage
                        .remove(&storage_name)
                        .await
                        .trace_expect(&format!(
                            "Failed to remove malformed bundle: {storage_name}"
                        ));
                    return (0, 1);
                }
            };
        drop(data);

        // Check if the metadata_storage knows about this bundle
        let metadata = metadata_storage
            .confirm_exists(&bundle.id)
            .await
            .trace_expect("Failed to confirm bundle existence");
        if let Some(metadata) = metadata {
            let drop = if let BundleStatus::Tombstone(_) = metadata.status {
                // Tombstone, ignore
                warn!("Tombstone bundle data found: {storage_name}");
                true
            } else if metadata.storage_name.as_ref() == Some(&storage_name) && metadata.hash == hash
            {
                false
            } else {
                warn!("Duplicate bundle data found: {storage_name}");
                true
            };

            if drop {
                // Remove spurious duplicate
                bundle_storage
                    .remove(&storage_name)
                    .await
                    .trace_expect(&format!(
                        "Failed to remove duplicate bundle: {storage_name}"
                    ));
                return (0, 1);
            }

            dispatcher
                .check_bundle(bundle::Bundle { metadata, bundle }, reason)
                .await
                .trace_expect(&format!("Bundle validation failed for: {storage_name}"));

            return (0, 0);
        }

        let mut bundle = bundle::Bundle {
            metadata: BundleMetadata {
                storage_name: Some(storage_name),
                hash,
                received_at: file_time,
                ..Default::default()
            },
            bundle,
        };

        // If the bundle isn't valid, it must always be a Tombstone
        if reason.is_some() {
            bundle.metadata.status = BundleStatus::Tombstone(time::OffsetDateTime::now_utc())
        }

        // Send to the dispatcher ingress as it is effectively a new bundle
        dispatcher
            .ingress_bundle(bundle, reason, report_unsupported)
            .await
            .trace_expect("Failed to restart bundle");

        (1, 0)
    }

    #[instrument(skip_all)]
    async fn check_waiting(
        wait_sample_interval: time::Duration,
        metadata_storage: Arc<dyn storage::MetadataStorage>,
        dispatcher: Arc<dispatcher::Dispatcher>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        while utils::cancellable_sleep(wait_sample_interval, &cancel_token).await {
            // Get all bundles that are ready before now() + self.config.wait_sample_interval
            let limit = time::OffsetDateTime::now_utc() + wait_sample_interval;

            let (tx, mut rx) =
                tokio::sync::mpsc::channel::<(metadata::BundleMetadata, bpv7::Bundle)>(16);
            let dispatcher = dispatcher.clone();
            let cancel_token = cancel_token.clone();

            let h = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        bundle = rx.recv() => match bundle {
                            None => break,
                            Some((m,b)) => {
                                // Double check returned bundles
                                match m.status {
                                    BundleStatus::ForwardAckPending(_, until)
                                    | BundleStatus::Waiting(until)
                                        if until <= limit =>
                                    {
                                        dispatcher.dispatch_bundle(bundle::Bundle{metadata:m,bundle:b}).await.trace_expect("Failed to dispatch bundle");
                                    }
                                    _ => {}
                                }
                            },
                        },
                        _ = cancel_token.cancelled() => {
                            rx.close();
                        }
                    }
                }
            });

            metadata_storage
                .get_waiting_bundles(limit, tx)
                .await
                .trace_expect("get_waiting_bundles failed");

            h.await.trace_expect("polling task failed")
        }
    }

    #[inline]
    pub async fn load_data(&self, storage_name: &str) -> storage::Result<Option<storage::DataRef>> {
        self.bundle_storage.load(storage_name).await
    }

    #[inline]
    pub async fn store_data(&self, data: &[u8]) -> storage::Result<(Arc<str>, Arc<[u8]>)> {
        // Calculate hash
        let hash = hash(data);

        // Write to bundle storage
        self.bundle_storage
            .store(data)
            .await
            .map(|storage_name| (storage_name, hash))
    }

    #[inline]
    pub async fn store_metadata(
        &self,
        metadata: &BundleMetadata,
        bundle: &bpv7::Bundle,
    ) -> storage::Result<bool> {
        // Write to metadata store
        Ok(self
            .metadata_storage
            .store(metadata, bundle)
            .await
            .trace_expect("Failed to store metadata"))
    }

    #[inline]
    pub async fn load(
        &self,
        bundle_id: &bpv7::BundleId,
    ) -> storage::Result<Option<bundle::Bundle>> {
        self.metadata_storage.load(bundle_id).await.map(|v| {
            v.map(|(m, b)| bundle::Bundle {
                metadata: m,
                bundle: b,
            })
        })
    }

    #[instrument(skip(self, data))]
    pub async fn store(
        &self,
        bundle: &bpv7::Bundle,
        data: &[u8],
        status: BundleStatus,
        received_at: Option<time::OffsetDateTime>,
    ) -> storage::Result<Option<BundleMetadata>> {
        // Write to bundle storage
        let (storage_name, hash) = self.store_data(data).await?;

        // Compose metadata
        let metadata = BundleMetadata {
            status,
            storage_name: Some(storage_name.clone()),
            hash: Some(hash),
            received_at,
        };

        // Write to metadata store
        match self.store_metadata(&metadata, bundle).await {
            Ok(true) => Ok(Some(metadata)),
            Ok(false) => {
                // We have a duplicate, remove the duplicate from the bundle store
                _ = self.bundle_storage.remove(&storage_name).await;
                Ok(None)
            }
            Err(e) => {
                // This is just bad, we can't really claim to have stored the bundle,
                // so just cleanup and get out
                _ = self.bundle_storage.remove(&storage_name).await;
                Err(e)
            }
        }
    }

    #[inline]
    pub async fn poll_for_collection(
        &self,
        destination: &bpv7::Eid,
        tx: storage::Sender,
    ) -> storage::Result<()> {
        self.metadata_storage
            .poll_for_collection(destination, tx)
            .await
    }

    #[inline]
    pub async fn check_status(
        &self,
        bundle_id: &bpv7::BundleId,
    ) -> storage::Result<Option<BundleStatus>> {
        self.metadata_storage.get_bundle_status(bundle_id).await
    }

    #[instrument(skip(self))]
    pub async fn set_status(
        &self,
        bundle: &mut bundle::Bundle,
        status: BundleStatus,
    ) -> storage::Result<()> {
        if bundle.metadata.status == status {
            Ok(())
        } else {
            bundle.metadata.status = status;
            self.metadata_storage
                .set_bundle_status(&bundle.bundle.id, &bundle.metadata.status)
                .await
        }
    }

    #[inline]
    pub async fn delete_data(&self, storage_name: &str) -> storage::Result<()> {
        // Delete the bundle from the bundle store
        self.bundle_storage.remove(storage_name).await
    }

    #[inline]
    pub async fn delete_metadata(&self, bundle_id: &bpv7::BundleId) -> storage::Result<()> {
        // Delete the bundle from the bundle store
        self.metadata_storage.remove(bundle_id).await
    }
}
