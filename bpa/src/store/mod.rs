use super::*;
use hardy_bpa_api::storage;
use hardy_cbor as cbor;
use sha2::Digest;
use std::sync::Arc;
use utils::settings;

#[cfg(feature = "mem-storage")]
mod metadata_mem;

#[cfg(feature = "mem-storage")]
mod bundle_mem;

fn hash(data: &[u8]) -> Arc<[u8]> {
    Arc::from(sha2::Sha256::digest(data).as_slice())
}

struct Config {
    wait_sample_interval: u64,
}

impl Config {
    fn new(config: &config::Config) -> Self {
        let config = Self {
            wait_sample_interval: settings::get_with_default(
                config,
                "wait_sample_interval",
                settings::WAIT_SAMPLE_INTERVAL_SECS,
            )
            .trace_expect("Invalid 'wait_sample_interval' value in configuration"),
        };

        if config.wait_sample_interval > i64::MAX as u64 {
            error!("wait_sample_interval is too large");
            panic!("wait_sample_interval is too large");
        }

        config
    }
}

pub struct Store {
    config: Config,
    metadata_storage: Arc<dyn storage::MetadataStorage>,
    bundle_storage: Arc<dyn storage::BundleStorage>,
}

fn init_metadata_storage(
    config: &config::Config,
    upgrade: bool,
) -> Arc<dyn storage::MetadataStorage> {
    cfg_if::cfg_if! {
        if #[cfg(feature = "sqlite-storage")] {
            const DEFAULT: &str = hardy_sqlite_storage::CONFIG_KEY;
        } else if #[cfg(feature = "mem-storage")] {
            const DEFAULT: &str = metadata_mem::CONFIG_KEY;
        } else {
            const DEFAULT: &str = "";
            compile_error!("No default metadata storage engine, rebuild the package with at least one metadata storage engine feature enabled");
        }
    }

    let engine: String = settings::get_with_default(config, "metadata_storage", DEFAULT)
        .trace_expect("Invalid 'metadata_storage' value in configuration");
    info!("Using '{engine}' metadata storage engine");

    let config = config.get_table(&engine).unwrap_or_default();
    match engine.as_str() {
        #[cfg(feature = "sqlite-storage")]
        hardy_sqlite_storage::CONFIG_KEY => hardy_sqlite_storage::Storage::init(&config, upgrade),

        #[cfg(feature = "mem-storage")]
        metadata_mem::CONFIG_KEY => metadata_mem::Storage::init(&config),

        _ => {
            error!("Unknown metadata storage engine: {engine}");
            panic!("Unknown metadata storage engine: {engine}")
        }
    }
}

fn init_bundle_storage(config: &config::Config, _upgrade: bool) -> Arc<dyn storage::BundleStorage> {
    cfg_if::cfg_if! {
        if #[cfg(feature = "localdisk-storage")] {
            const DEFAULT: &str = hardy_localdisk_storage::CONFIG_KEY;
        } else if #[cfg(feature = "mem-storage")] {
            const DEFAULT: &str = bundle_mem::CONFIG_KEY;
        } else {
            const DEFAULT: &str = "";
            compile_error!("No default bundle storage engine, rebuild the package with at least one bundle storage engine feature enabled");
        }
    }

    let engine: String = settings::get_with_default(config, "bundle_storage", DEFAULT)
        .trace_expect("Invalid 'bundle_storage' value in configuration");
    info!("Using '{engine}' bundle storage engine");

    let config = config.get_table(&engine).unwrap_or_default();
    match engine.as_str() {
        #[cfg(feature = "localdisk-storage")]
        hardy_localdisk_storage::CONFIG_KEY => hardy_localdisk_storage::Storage::init(&config),

        #[cfg(feature = "mem-storage")]
        bundle_mem::CONFIG_KEY => bundle_mem::Storage::init(&config),

        _ => {
            error!("Unknown bundle storage engine: {engine}");
            panic!("Unknown bundle storage engine: {engine}")
        }
    }
}

impl Store {
    pub fn new(config: &config::Config, upgrade: bool) -> Arc<Self> {
        // Init pluggable storage engines
        Arc::new(Self {
            config: Config::new(config),
            metadata_storage: init_metadata_storage(config, upgrade),
            bundle_storage: init_bundle_storage(config, upgrade),
        })
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

        if !cancel_token.is_cancelled() {
            // Now check the metadata storage for old data
            self.metadata_storage_check(dispatcher.clone(), cancel_token.clone())
                .await;

            if !cancel_token.is_cancelled() {
                info!("Store restarted");

                // Spawn a waiter
                let wait_sample_interval =
                    time::Duration::seconds(self.config.wait_sample_interval as i64);
                let metadata_storage = self.metadata_storage.clone();
                task_set.spawn(Self::check_waiting(
                    wait_sample_interval,
                    metadata_storage,
                    dispatcher,
                    cancel_token.clone(),
                ));
            }
        }
    }

    #[instrument(skip_all)]
    async fn metadata_storage_check(
        &self,
        dispatcher: Arc<dispatcher::Dispatcher>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<metadata::Bundle>(16);
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
                        Some(bundle) => {
                            if let metadata::BundleStatus::Tombstone(_) = &bundle.metadata.status {
                                // Ignore Tombstones
                            } else {
                                bundles = bundles.saturating_add(1);

                                // The data associated with `bundle` has gone!
                                dispatcher.report_bundle_deletion(
                                    &bundle,
                                    bpv7::StatusReportReasonCode::DepletedStorage,
                                )
                                .await.trace_expect("Failed to report bundle deletion");

                                // Delete it
                                metadata_storage
                                    .remove(&bundle.bundle.id)
                                    .await.trace_expect("Failed to remove orphan bundle")
                            }
                        }
                    },
                    _ = cancel_token.cancelled() => break,
                }
            }
        });

        self.metadata_storage
            .get_unconfirmed_bundles(tx)
            .await
            .trace_expect("Failed to get unconfirmed bundles");

        h.await.trace_expect("Task terminated unexpectedly")
    }

    #[instrument(skip_all)]
    async fn list_bundles(
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
                    _ = cancel_token.cancelled() => break
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
        for (storage_name, file_time) in self.list_bundles(cancel_token.clone()).await {
            bundles = bundles.saturating_add(1);

            loop {
                tokio::select! {
                    () = &mut timer => {
                        info!("Bundle restart in progress, {bundles} bundles processed, {orphans} orphan and {bad} bad bundles found");
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
        info!("Bundle restart complete, {bundles} bundles processed, {orphans} orphan and {bad} bad bundles found");
    }

    async fn restart_bundle(
        metadata_storage: Arc<dyn storage::MetadataStorage>,
        bundle_storage: Arc<dyn storage::BundleStorage>,
        dispatcher: Arc<dispatcher::Dispatcher>,
        storage_name: Arc<str>,
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

        // Calculate hash
        let hash = Some(hash(data.as_ref().as_ref()));

        // Parse the bundle
        let parse_result = cbor::decode::parse::<bpv7::ValidBundle>(data.as_ref().as_ref());

        drop(data);

        let (bundle, valid) = match parse_result {
            Ok(bpv7::ValidBundle::Valid(bundle)) => (bundle, true),
            Ok(bpv7::ValidBundle::Invalid(bundle)) => (bundle, false),
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

        // Check if the metadata_storage knows about this bundle
        let metadata = metadata_storage
            .confirm_exists(&bundle.id)
            .await
            .trace_expect("Failed to confirm bundle existence");
        if let Some(metadata) = &metadata {
            let drop = if let metadata::BundleStatus::Tombstone(_) = metadata.status {
                // Tombstone, ignore
                warn!("Tombstone bundle data found: {storage_name}");
                true
            } else if metadata.storage_name == Some(storage_name.clone()) && metadata.hash == hash {
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
        }
        let orphan = metadata.is_none();

        // Send to the dispatcher to sort out
        dispatcher
            .restart_bundle(
                metadata::Bundle {
                    metadata: metadata.unwrap_or(metadata::Metadata {
                        storage_name: Some(storage_name),
                        hash,
                        received_at: file_time,
                        ..Default::default()
                    }),
                    bundle,
                },
                valid,
                orphan,
            )
            .await
            .trace_expect("Failed to restart bundle");

        (if orphan { 1 } else { 0 }, 0)
    }

    #[instrument(skip_all)]
    async fn check_waiting(
        wait_sample_interval: time::Duration, //time::Duration::seconds(self.config.wait_sample_interval as i64)
        metadata_storage: Arc<dyn storage::MetadataStorage>,
        dispatcher: Arc<dispatcher::Dispatcher>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        while utils::cancel::cancellable_sleep(wait_sample_interval, &cancel_token).await {
            // Get all bundles that are ready before now() + self.config.wait_sample_interval
            let limit = time::OffsetDateTime::now_utc() + wait_sample_interval;
            let (tx, mut rx) = tokio::sync::mpsc::channel::<metadata::Bundle>(16);
            let dispatcher = dispatcher.clone();
            let cancel_token = cancel_token.clone();
            let h = tokio::spawn(async move {
                // We're going to spawn a bunch of tasks
                let mut task_set = tokio::task::JoinSet::new();

                loop {
                    tokio::select! {
                        bundle = rx.recv() => match bundle {
                            None => break,
                            Some(bundle) => {
                                // Double check returned bundles
                                match bundle.metadata.status {
                                    metadata::BundleStatus::ForwardAckPending(_, until)
                                    | metadata::BundleStatus::Waiting(until)
                                        if until <= limit =>
                                    {
                                        // Spawn a task for each ready bundle
                                        let dispatcher = dispatcher.clone();
                                        task_set.spawn(async move {
                                            dispatcher.delay_bundle(bundle, until).await.trace_expect("Failed to delay bundle");
                                        });
                                    }
                                    _ => {}
                                }
                            },
                        },
                        Some(r) = task_set.join_next(), if !task_set.is_empty() => r.trace_expect("Task terminated unexpectedly"),
                        _ = cancel_token.cancelled() => break,
                    }
                }

                // Wait for all sub-tasks to complete
                while let Some(r) = task_set.join_next().await {
                    r.trace_expect("Task terminated unexpectedly")
                }
            });

            metadata_storage
                .get_waiting_bundles(limit, tx)
                .await
                .trace_expect("get_waiting_bundles failed");

            h.await.trace_expect("polling task failed")
        }
    }

    pub async fn load_data(&self, storage_name: &str) -> Result<Option<storage::DataRef>, Error> {
        self.bundle_storage.load(storage_name).await
    }

    pub async fn store_data(&self, data: Box<[u8]>) -> Result<(Arc<str>, Arc<[u8]>), Error> {
        // Calculate hash
        let hash = hash(&data);

        // Write to bundle storage
        self.bundle_storage
            .store(data)
            .await
            .map(|storage_name| (storage_name, hash))
    }

    pub async fn store_metadata(
        &self,
        metadata: &metadata::Metadata,
        bundle: &bpv7::Bundle,
    ) -> Result<bool, Error> {
        // Write to metadata store
        Ok(self
            .metadata_storage
            .store(metadata, bundle)
            .await
            .trace_expect("Failed to store metadata"))
    }

    pub async fn load(
        &self,
        bundle_id: &bpv7::BundleId,
    ) -> Result<Option<metadata::Bundle>, Error> {
        self.metadata_storage.load(bundle_id).await
    }

    #[instrument(skip(self, data))]
    pub async fn store(
        &self,
        bundle: &bpv7::Bundle,
        data: Box<[u8]>,
        status: metadata::BundleStatus,
        received_at: Option<time::OffsetDateTime>,
    ) -> Result<Option<metadata::Metadata>, Error> {
        // Write to bundle storage
        let (storage_name, hash) = self.store_data(data).await?;

        // Compose metadata
        let metadata = metadata::Metadata {
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

    #[instrument(skip(self))]
    pub async fn poll_for_collection(
        &self,
        destination: bpv7::Eid,
        tx: tokio::sync::mpsc::Sender<metadata::Bundle>,
    ) -> Result<(), Error> {
        self.metadata_storage
            .poll_for_collection(destination, tx)
            .await
    }

    #[instrument(skip(self))]
    pub async fn check_status(
        &self,
        bundle_id: &bpv7::BundleId,
    ) -> Result<Option<metadata::BundleStatus>, Error> {
        self.metadata_storage.get_bundle_status(bundle_id).await
    }

    #[instrument(skip(self))]
    pub async fn set_status(
        &self,
        bundle: &mut metadata::Bundle,
        status: metadata::BundleStatus,
    ) -> Result<(), Error> {
        if bundle.metadata.status == status {
            Ok(())
        } else {
            bundle.metadata.status = status;
            self.metadata_storage
                .set_bundle_status(&bundle.bundle.id, &bundle.metadata.status)
                .await
        }
    }

    #[instrument(skip(self))]
    pub async fn delete_data(&self, storage_name: &str) -> Result<(), Error> {
        // Delete the bundle from the bundle store
        self.bundle_storage.remove(storage_name).await
    }

    #[instrument(skip(self))]
    pub async fn delete_metadata(&self, bundle_id: &bpv7::BundleId) -> Result<(), Error> {
        // Delete the bundle from the bundle store
        self.metadata_storage.remove(bundle_id).await
    }
}
