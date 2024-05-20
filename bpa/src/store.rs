use super::*;
use hardy_bpa_core::storage;
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
        self.metadata_storage.restart(&mut |metadata, bundle| {
            tokio::runtime::Handle::current().block_on(async {
                // Just shove bundles into the Ingress
                ingress.recheck_bundle(metadata, bundle).await
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
        self.metadata_storage
            .check_orphans(&mut |metadata, bundle| {
                tokio::runtime::Handle::current().block_on(async {
                    // The data associated with `bundle` has gone!
                    dispatcher
                        .report_bundle_deleted(
                            &metadata,
                            &bundle,
                            bundle::StatusReportReasonCode::DepletedStorage,
                        )
                        .await?;

                    // Delete it
                    self.metadata_storage
                        .remove(&metadata.storage_name)
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

    pub async fn load_data(&self, storage_name: &str) -> Result<storage::DataRef, Error> {
        self.bundle_storage.load(storage_name).await
    }

    pub async fn store_data(&self, data: Vec<u8>) -> Result<String, Error> {
        // Write to bundle storage
        self.bundle_storage.store(data).await
    }

    pub async fn store_metadata(
        &self,
        metadata: &bundle::Metadata,
        bundle: &bundle::Bundle,
    ) -> Result<bool, Error> {
        // Write to metadata store
        self.metadata_storage.store(metadata, bundle).await
    }

    pub async fn load(
        &self,
        bundle_id: &bundle::BundleId,
    ) -> Result<Option<(bundle::Metadata, bundle::Bundle)>, Error> {
        self.metadata_storage.load(bundle_id).await
    }

    #[instrument(skip(self, data))]
    pub async fn store(
        &self,
        bundle: &bundle::Bundle,
        data: Vec<u8>,
        status: bundle::BundleStatus,
        received_at: Option<time::OffsetDateTime>,
    ) -> Result<Option<bundle::Metadata>, Error> {
        // Calculate hash
        let hash = self.hash(&data);

        // Write to bundle storage
        let storage_name = self.bundle_storage.store(data).await?;

        // Compose metadata
        let metadata = bundle::Metadata {
            status,
            storage_name,
            hash: hash.to_vec(),
            received_at,
        };

        // Write to metadata store
        match self.metadata_storage.store(&metadata, bundle).await {
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
    ) -> Result<Vec<(bundle::Metadata, bundle::Bundle, time::OffsetDateTime)>, Error> {
        self.metadata_storage.get_waiting_bundles(limit).await
    }

    pub async fn poll_for_collection(
        &self,
        destination: bundle::Eid,
    ) -> Result<Vec<(String, time::OffsetDateTime)>, Error> {
        self.metadata_storage
            .poll_for_collection(destination)
            .await
            .map(|v| {
                v.into_iter()
                    .filter_map(|(metadata, bundle)| {
                        // Double check that we are returning something valid
                        if let bundle::BundleStatus::CollectionPending = &metadata.status {
                            let expiry = bundle::get_bundle_expiry(&metadata, &bundle);
                            if expiry > time::OffsetDateTime::now_utc() {
                                return Some((bundle.id.to_key(), expiry));
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
        metadata: &bundle::Metadata,
        data: Vec<u8>,
    ) -> Result<bundle::Metadata, Error> {
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

        Ok(bundle::Metadata {
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
    ) -> Result<Option<bundle::BundleStatus>, Error> {
        self.metadata_storage
            .check_bundle_status(storage_name)
            .await
    }

    pub async fn set_status(
        &self,
        storage_name: &str,
        status: &bundle::BundleStatus,
    ) -> Result<(), Error> {
        self.metadata_storage
            .set_bundle_status(storage_name, status)
            .await
    }

    #[instrument(skip(self))]
    pub async fn delete(&self, storage_name: &str) -> Result<(), Error> {
        // Entirely delete the bundle from the metadata and bundle stores
        self.bundle_storage.remove(storage_name).await?;
        self.metadata_storage.remove(storage_name).await?;
        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn remove(&self, storage_name: &str) -> Result<(), Error> {
        // Delete the bundle from the bundle store
        self.bundle_storage.remove(storage_name).await?;

        // But leave a tombstone in the metadata, so we can ignore duplicates
        self.metadata_storage
            .set_bundle_status(storage_name, &bundle::BundleStatus::Tombstone)
            .await?;
        Ok(())
    }
}
