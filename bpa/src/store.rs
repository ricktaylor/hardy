use super::*;
use hardy_bpa_core::storage;
use sha2::Digest;
use std::sync::Arc;

pub struct Store {
    metadata_storage: Arc<dyn storage::MetadataStorage>,
    bundle_storage: Arc<dyn storage::BundleStorage>,
}

impl Clone for Store {
    fn clone(&self) -> Self {
        Self {
            metadata_storage: self.metadata_storage.clone(),
            bundle_storage: self.bundle_storage.clone(),
        }
    }
}

fn init_metadata_storage(
    config: &config::Config,
    upgrade: bool,
) -> Result<std::sync::Arc<dyn storage::MetadataStorage>, anyhow::Error> {
    const DEFAULT: &str = if cfg!(feature = "sqlite-storage") {
        hardy_sqlite_storage::CONFIG_KEY
    } else {
        panic!("No default metadata storage engine, rebuild the package with at least one metadata storage engine feature enabled");
    };

    let engine: String = settings::get_with_default(config, "metadata_storage", DEFAULT)
        .map_err(|e| anyhow!("Failed to parse 'metadata_storage' config param: {}", e))?;

    let config = config.get_table(&engine).unwrap_or_default();

    log::info!("Using metadata storage: {}", engine);

    match engine.as_str() {
        #[cfg(feature = "sqlite-storage")]
        hardy_sqlite_storage::CONFIG_KEY => hardy_sqlite_storage::Storage::init(&config, upgrade),

        _ => Err(anyhow!("Unknown metadata storage engine {}", engine)),
    }
}

fn init_bundle_storage(
    config: &config::Config,
    _upgrade: bool,
) -> Result<std::sync::Arc<dyn storage::BundleStorage>, anyhow::Error> {
    const DEFAULT: &str = if cfg!(feature = "localdisk-storage") {
        hardy_localdisk_storage::CONFIG_KEY
    } else {
        panic!("No default bundle storage engine, rebuild the package with at least one bundle storage engine feature enabled");
    };

    let engine: String = settings::get_with_default(config, "bundle_storage", DEFAULT)
        .map_err(|e| anyhow!("Failed to parse 'bundle_storage' config param: {}", e))?;
    let config = config.get_table(&engine).unwrap_or_default();

    log::info!("Using bundle storage: {}", engine);

    match engine.as_str() {
        #[cfg(feature = "localdisk-storage")]
        hardy_localdisk_storage::CONFIG_KEY => hardy_localdisk_storage::Storage::init(&config),

        _ => Err(anyhow!("Unknown bundle storage engine {}", engine)),
    }
}

impl Store {
    pub fn new(config: &config::Config, upgrade: bool) -> Self {
        // Init pluggable storage engines
        Self {
            metadata_storage: init_metadata_storage(config, upgrade)
                .log_expect("Failed to initialize metadata store"),
            bundle_storage: init_bundle_storage(config, upgrade)
                .log_expect("Failed to initialize bundle store"),
        }
    }

    pub async fn restart(
        &self,
        ingress: ingress::Ingress,
        dispatcher: dispatcher::Dispatcher,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<(), anyhow::Error> {
        let self_cloned = self.clone();
        tokio::task::spawn_blocking(move || {
            // Bundle storage check first
            log::info!("Starting store consistency check...");
            self_cloned.bundle_storage_check(ingress.clone(), cancel_token.clone())?;

            // Now check the metadata storage for orphans
            if !cancel_token.is_cancelled() {
                self_cloned.metadata_storage_check(dispatcher, cancel_token.clone())?;
                if !cancel_token.is_cancelled() {
                    log::info!("Store consistency check complete");

                    // Now restart the store
                    log::info!("Restarting store...");
                    self_cloned.metadata_storage_restart(ingress, cancel_token.clone())?;

                    if !cancel_token.is_cancelled() {
                        log::info!("Store restart complete");
                    }
                }
            }

            Ok::<(), anyhow::Error>(())
        })
        .await?
    }

    fn metadata_storage_restart(
        &self,
        ingress: ingress::Ingress,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<(), anyhow::Error> {
        self.metadata_storage.restart(&mut |metadata, bundle| {
            tokio::runtime::Handle::current().block_on(async {
                // Just shove bundles into the Ingress
                ingress.enqueue_ingress_bundle(metadata, bundle).await
            })?;

            // Just dumb poll the cancel token now - try to avoid mismatched state again
            Ok(!cancel_token.is_cancelled())
        })
    }

    fn metadata_storage_check(
        &self,
        dispatcher: dispatcher::Dispatcher,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<(), anyhow::Error> {
        self.metadata_storage
            .check_orphans(&mut |metadata, bundle| {
                tokio::runtime::Handle::current().block_on(async {
                    // The data associated with `bundle` has gone!
                    dispatcher
                        .report_bundle_deletion(
                            &metadata,
                            &bundle,
                            dispatcher::BundleStatusReportReasonCode::DepletedStorage,
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

    fn bundle_storage_check(
        &self,
        ingress: ingress::Ingress,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<(), anyhow::Error> {
        self.bundle_storage
            .check_orphans(&mut |storage_name, file_time| {
                tokio::runtime::Handle::current().block_on(async {
                    // Check if the metadata_storage knows about this bundle
                    if !self
                        .metadata_storage
                        .confirm_exists(storage_name, None)
                        .await?
                    {
                        // Queue the new bundle for receive processing
                        ingress
                            .enqueue_receive_bundle(storage_name, file_time)
                            .await?;
                    }
                    Ok::<(), anyhow::Error>(())
                })?;

                // Just dumb poll the cancel token now - try to avoid mismatched state again
                Ok(!cancel_token.is_cancelled())
            })
    }

    pub async fn load_data(
        &self,
        storage_name: &str,
    ) -> Result<Arc<dyn AsRef<[u8]>>, anyhow::Error> {
        self.bundle_storage.load(storage_name).await
    }

    pub async fn store_data(&self, data: Vec<u8>) -> Result<String, anyhow::Error> {
        // Write to bundle storage
        Ok(self.bundle_storage.store(data).await?)
    }

    pub async fn store_metadata(
        &self,
        metadata: &bundle::Metadata,
        bundle: &bundle::Bundle,
    ) -> Result<(), anyhow::Error> {
        // Write to metadata store
        self.metadata_storage.store(metadata, bundle).await
    }

    pub async fn set_status(
        &self,
        storage_name: &str,
        status: bundle::BundleStatus,
    ) -> Result<(), anyhow::Error> {
        self.metadata_storage
            .set_bundle_status(storage_name, status)
            .await
    }

    pub async fn remove(&self, storage_name: &str) -> Result<(), anyhow::Error> {
        self.bundle_storage.remove(storage_name).await?;
        self.metadata_storage.remove(storage_name).await?;
        Ok(())
    }
}
