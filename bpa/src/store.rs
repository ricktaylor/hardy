use super::*;
use hardy_bpa_api::storage;
use std::sync::Arc;
use utils::settings;

#[derive(Clone)]
pub struct Store {
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
            metadata_storage: init_metadata_storage(config, upgrade),
            bundle_storage: init_bundle_storage(config, upgrade),
        }
    }

    pub fn hash(&self, data: &[u8]) -> Vec<u8> {
        self.bundle_storage.hash(data)
    }

    #[instrument(skip_all)]
    pub async fn restart(
        &self,
        ingress: ingress::Ingress,
        dispatcher: dispatcher::Dispatcher,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        info!("Starting store consistency check...");
        self.bundle_storage_check(ingress.clone(), cancel_token.clone())
            .trace_expect("Bundle storage consistency check failed");

        // Now check the metadata storage for orphans
        if !cancel_token.is_cancelled() {
            self.metadata_storage_check(dispatcher, cancel_token.clone())
                .trace_expect("Metadata storage consistency check failed");
            if !cancel_token.is_cancelled() {
                info!("Store consistency check complete");

                // Now restart the store
                info!("Restarting store...");
                self.metadata_storage_restart(ingress, cancel_token.clone())
                    .trace_expect("Store restart failed");

                if !cancel_token.is_cancelled() {
                    info!("Store restarted");
                }
            }
        }
    }

    #[instrument(skip_all)]
    fn metadata_storage_restart(
        &self,
        ingress: ingress::Ingress,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<(), Error> {
        self.metadata_storage.restart(&mut |bundle| {
            tokio::runtime::Handle::current().block_on(async {
                // Just shove bundles into the Ingress
                ingress.recheck_bundle(bundle).await
            })?;

            // Just dumb poll the cancel token now - try to avoid mismatched state again
            Ok(!cancel_token.is_cancelled())
        })
    }

    #[instrument(skip_all)]
    fn metadata_storage_check(
        &self,
        dispatcher: dispatcher::Dispatcher,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<(), Error> {
        self.metadata_storage.check_orphans(&mut |bundle| {
            tokio::runtime::Handle::current().block_on(async {
                // The data associated with `bundle` has gone!
                dispatcher
                    .report_bundle_deletion(&bundle, bpv7::StatusReportReasonCode::DepletedStorage)
                    .await?;

                // Delete it
                self.metadata_storage
                    .remove(&bundle.metadata.storage_name)
                    .await
                    .map(|_| ())
            })?;

            // Just dumb poll the cancel token now - try to avoid mismatched state again
            Ok(!cancel_token.is_cancelled())
        })
    }

    #[instrument(skip_all)]
    fn bundle_storage_check(
        &self,
        ingress: ingress::Ingress,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<(), Error> {
        self.bundle_storage
            .check_orphans(&mut |storage_name, hash, file_time| {
                tokio::runtime::Handle::current().block_on(async {
                    // Check if the metadata_storage knows about this bundle
                    if !self
                        .metadata_storage
                        .confirm_exists(storage_name, hash)
                        .await?
                    {
                        // Queue the new bundle for receive processing
                        ingress
                            .enqueue_receive_bundle(storage_name, file_time)
                            .await?;
                    }
                    Ok::<_, storage::Error>(())
                })?;

                // Just dumb poll the cancel token now - try to avoid mismatched state again
                Ok(!cancel_token.is_cancelled())
            })
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

    pub async fn store_data(&self, data: Vec<u8>) -> Result<String, Error> {
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
        data: Vec<u8>,
        status: metadata::BundleStatus,
        received_at: Option<time::OffsetDateTime>,
    ) -> Result<Option<metadata::Metadata>, Error> {
        // Calculate hash
        let hash = self.hash(&data);

        // Write to bundle storage
        let storage_name = self.store_data(data).await?;

        // Compose metadata
        let metadata = metadata::Metadata {
            status,
            storage_name,
            hash: hash.to_vec(),
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

    pub async fn get_waiting_bundles(
        &self,
        limit: time::OffsetDateTime,
    ) -> Result<Vec<(metadata::Bundle, time::OffsetDateTime)>, Error> {
        self.metadata_storage.get_waiting_bundles(limit).await
    }

    pub async fn poll_for_collection(
        &self,
        destination: bpv7::Eid,
    ) -> Result<Vec<(String, time::OffsetDateTime)>, Error> {
        self.metadata_storage
            .poll_for_collection(destination)
            .await
            .map(|v| {
                v.into_iter()
                    .filter_map(|bundle| {
                        // Double check that we are returning something valid
                        if let metadata::BundleStatus::CollectionPending = &bundle.metadata.status {
                            let expiry = bundle.expiry();
                            if expiry > time::OffsetDateTime::now_utc() {
                                return Some((bundle.bundle.id.to_key(), expiry));
                            }
                        }
                        None
                    })
                    .collect()
            })
    }

    #[instrument(skip(self, data))]
    pub async fn replace_data(
        &self,
        metadata: &metadata::Metadata,
        data: Vec<u8>,
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
            hash: hash.to_vec(),
            received_at: metadata.received_at,
        })
    }

    #[instrument(skip(self))]
    pub async fn check_status(
        &self,
        storage_name: &str,
    ) -> Result<Option<metadata::BundleStatus>, Error> {
        self.metadata_storage
            .check_bundle_status(storage_name)
            .await
    }

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

    #[instrument(skip(self))]
    pub async fn remove(&self, storage_name: &str) -> Result<(), Error> {
        // Delete the bundle from the bundle store
        self.bundle_storage.remove(storage_name).await?;

        // But leave a tombstone in the metadata, so we can ignore duplicates
        self.metadata_storage
            .set_bundle_status(storage_name, &metadata::BundleStatus::Tombstone)
            .await?;
        Ok(())
    }
}
