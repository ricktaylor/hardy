use super::*;
use hardy_bpa_api::storage;
use std::sync::Arc;
use utils::settings;

#[cfg(feature = "mem-storage")]
mod metadata_mem;

#[cfg(feature = "mem-storage")]
mod bundle_mem;

struct DataRefWrapper(Arc<[u8]>);

impl AsRef<[u8]> for DataRefWrapper {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref()
    }
}

pub fn into_dataref(data: Arc<[u8]>) -> Arc<dyn AsRef<[u8]> + Send + Sync> {
    Arc::new(DataRefWrapper(data))
}

#[derive(Clone)]
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

#[derive(Clone)]
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
    pub fn new(config: &config::Config, upgrade: bool) -> Self {
        // Init pluggable storage engines
        Self {
            config: Config::new(config),
            metadata_storage: init_metadata_storage(config, upgrade),
            bundle_storage: init_bundle_storage(config, upgrade),
        }
    }

    pub fn hash(&self, data: &[u8]) -> Arc<[u8]> {
        self.bundle_storage.hash(data)
    }

    #[instrument(skip_all)]
    pub async fn start(
        &self,
        ingress: ingress::Ingress,
        dispatcher: dispatcher::Dispatcher,
        task_set: &mut tokio::task::JoinSet<()>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        info!("Starting store consistency check...");
        self.bundle_storage_check(ingress.clone(), cancel_token.clone())
            .await;

        // Now check the metadata storage for orphans
        if !cancel_token.is_cancelled() {
            self.metadata_storage_check(dispatcher.clone(), cancel_token.clone())
                .await;

            if !cancel_token.is_cancelled() {
                info!("Store consistency check complete");

                // Now restart the store
                info!("Restarting store...");
                self.metadata_storage_restart(ingress, cancel_token.clone())
                    .await;

                if !cancel_token.is_cancelled() {
                    info!("Store restarted");

                    // Spawn a waiter
                    let self_cloned = self.clone();
                    task_set.spawn(Self::check_waiting(
                        self_cloned,
                        dispatcher,
                        cancel_token.clone(),
                    ));
                }
            }
        }
    }

    #[instrument(skip_all)]
    async fn metadata_storage_restart(
        &self,
        ingress: ingress::Ingress,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<metadata::Bundle>(16);
        let h = tokio::spawn(async move {
            loop {
                tokio::select! {
                    bundle = rx.recv() => match bundle {
                        None => break,
                        Some(bundle) => {
                            if let metadata::BundleStatus::Tombstone(_) = &bundle.metadata.status {
                                // Ignore Tombstones
                            } else {
                                // Just shove bundles into the Ingress
                                ingress.recheck_bundle(bundle).await.trace_expect("Failed to feed restart bundle into ingress")
                            }
                        }
                    },
                    _ = cancel_token.cancelled() => break
                }
            }
        });

        self.metadata_storage
            .restart(tx)
            .await
            .trace_expect("Failed to restart metadata storage");

        h.await.trace_expect("Task terminated unexpectedly")
    }

    #[instrument(skip_all)]
    async fn metadata_storage_check(
        &self,
        dispatcher: dispatcher::Dispatcher,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<metadata::Bundle>(16);
        let self_cloned = self.clone();
        let h = tokio::spawn(async move {
            // We're going to spawn a bunch of tasks
            let mut task_set = tokio::task::JoinSet::new();

            loop {
                tokio::select! {
                    bundle = rx.recv() => match bundle {
                        None => break,
                        Some(bundle) => {
                            if let metadata::BundleStatus::Tombstone(_) = &bundle.metadata.status {
                                // Ignore Tombstones
                            } else {
                                // The data associated with `bundle` has gone!
                                let dispatcher_cloned = dispatcher.clone();
                                let self_cloned = self_cloned.clone();
                                task_set.spawn(async move {
                                    dispatcher_cloned.report_bundle_deletion(
                                        &bundle,
                                        bpv7::StatusReportReasonCode::DepletedStorage,
                                    )
                                    .await.trace_expect("Failed to report bundle deletion");

                                    // Delete it
                                    self_cloned.metadata_storage
                                    .remove(&bundle.metadata.storage_name)
                                    .await.trace_expect("Failed to remove orphan bundle")
                                });
                            }
                        }
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

        self.metadata_storage
            .get_unconfirmed_bundles(tx)
            .await
            .trace_expect("Failed to get unconfirmed bundles");

        h.await.trace_expect("Task terminated unexpectedly")
    }

    #[instrument(skip_all)]
    async fn bundle_storage_check(
        &self,
        ingress: ingress::Ingress,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(
            Arc<str>,
            Arc<[u8]>,
            storage::DataRef,
            Option<time::OffsetDateTime>,
        )>(16);
        let self_cloned = self.clone();

        let h = tokio::spawn(async move {
            loop {
                tokio::select! {
                    r = rx.recv() => match r {
                        None => break,
                        Some((storage_name,hash,data,file_time)) => {
                            // Check if the metadata_storage knows about this bundle
                            if !self_cloned
                                .metadata_storage
                                .confirm_exists(&storage_name, &hash)
                                .await.trace_expect("Failed to confirm bundle existence")
                            {
                                // Queue the new bundle for receive processing
                                ingress
                                    .enqueue_receive_bundle(storage_name,hash,data,file_time)
                                    .await.trace_expect("Failed to enqueue orphan bundle");
                            }
                        }
                    },
                    _ = cancel_token.cancelled() => break,
                }
            }
        });

        self.bundle_storage
            .list(tx)
            .await
            .trace_expect("Failed to get stored bundles");

        h.await.trace_expect("Task terminated unexpectedly")
    }

    #[instrument(skip_all)]
    async fn check_waiting(
        self,
        dispatcher: dispatcher::Dispatcher,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        while utils::cancel::cancellable_sleep(
            time::Duration::seconds(self.config.wait_sample_interval as i64),
            &cancel_token,
        )
        .await
        {
            // Get all bundles that are ready before now() + self.config.wait_sample_interval
            let limit = time::OffsetDateTime::now_utc()
                + time::Duration::new(self.config.wait_sample_interval as i64, 0);
            let (tx, mut rx) = tokio::sync::mpsc::channel::<metadata::Bundle>(16);
            let dispatcher_cloned = dispatcher.clone();
            let cancel_token_cloned = cancel_token.clone();
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
                                        let dispatcher_cloned = dispatcher_cloned.clone();
                                        task_set.spawn(async move {
                                            dispatcher_cloned.delay_bundle(bundle, until).await.trace_expect("Failed to delay bundle");
                                        });
                                    }
                                    _ => {}
                                }
                            },
                        },
                        Some(r) = task_set.join_next(), if !task_set.is_empty() => r.trace_expect("Task terminated unexpectedly"),
                        _ = cancel_token_cloned.cancelled() => break,
                    }
                }

                // Wait for all sub-tasks to complete
                while let Some(r) = task_set.join_next().await {
                    r.trace_expect("Task terminated unexpectedly")
                }
            });

            self.metadata_storage
                .get_waiting_bundles(limit, tx)
                .await
                .trace_expect("get_waiting_bundles failed");

            h.await.trace_expect("polling task failed")
        }
    }

    pub async fn load_data(&self, storage_name: &str) -> Result<Option<storage::DataRef>, Error> {
        // Try to load the data, but treat errors as 'Storage Depleted'
        match self.bundle_storage.load(storage_name).await {
            Ok(data) => Ok(Some(data)),
            Err(e) => {
                warn!("Failed to load bundle data from bundle_store: {e}");

                // Hard delete the record from the metadata store, we lost it somehow
                self.metadata_storage
                    .remove(storage_name)
                    .await
                    .map(|_| None)
            }
        }
    }

    pub async fn store_data(&self, data: Arc<[u8]>) -> Result<(Arc<str>, Arc<[u8]>), Error> {
        // Write to bundle storage
        self.bundle_storage.store(data).await
    }

    pub async fn store_metadata(
        &self,
        metadata: &metadata::Metadata,
        bundle: &bpv7::Bundle,
    ) -> Result<bool, Error> {
        // Write to metadata store
        self.metadata_storage.store(metadata, bundle).await
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
        data: Arc<[u8]>,
        status: metadata::BundleStatus,
        received_at: Option<time::OffsetDateTime>,
    ) -> Result<Option<metadata::Metadata>, Error> {
        // Write to bundle storage
        let (storage_name, hash) = self.store_data(data).await?;

        // Compose metadata
        let metadata = metadata::Metadata {
            status,
            storage_name,
            hash,
            received_at,
        };

        // Write to metadata store
        match self.store_metadata(&metadata, bundle).await {
            Ok(true) => Ok(Some(metadata)),
            Ok(false) => {
                // We have a duplicate, remove the duplicate from the bundle store
                let _ = self.bundle_storage.remove(&metadata.storage_name).await;
                Ok(None)
            }
            Err(e) => {
                // This is just bad, we can't really claim to have stored the bundle,
                // so just cleanup and get out
                let _ = self.bundle_storage.remove(&metadata.storage_name).await;
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

    #[instrument(skip(self, data))]
    pub async fn replace_data(
        &self,
        metadata: &metadata::Metadata,
        data: Box<[u8]>,
    ) -> Result<metadata::Metadata, Error> {
        // Calculate hash
        let hash = self.hash(&data);

        // Let the metadata storage know we are about to replace a bundle
        self.metadata_storage
            .begin_replace(&metadata.storage_name, &hash)
            .await?;

        // Store the new data
        self.bundle_storage
            .replace(&metadata.storage_name, data)
            .await?;

        // Update any replacement tracking in the metadata store
        self.metadata_storage
            .commit_replace(&metadata.storage_name, &hash)
            .await?;

        Ok(metadata::Metadata {
            status: metadata.status.clone(),
            storage_name: metadata.storage_name.clone(),
            hash,
            received_at: metadata.received_at,
        })
    }

    #[instrument(skip(self))]
    pub async fn check_status(
        &self,
        storage_name: &str,
    ) -> Result<Option<metadata::BundleStatus>, Error> {
        self.metadata_storage.get_bundle_status(storage_name).await
    }

    #[instrument(skip(self))]
    pub async fn set_status(
        &self,
        storage_name: &str,
        status: &metadata::BundleStatus,
    ) -> Result<(), Error> {
        self.metadata_storage
            .set_bundle_status(storage_name, status)
            .await
    }

    #[instrument(skip(self))]
    pub async fn delete_data(&self, storage_name: &str) -> Result<(), Error> {
        // Delete the bundle from the bundle store
        self.bundle_storage.remove(storage_name).await
    }
}
