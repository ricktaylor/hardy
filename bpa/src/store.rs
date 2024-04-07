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
            log::info!("Starting store consistency check...");

            // Bundle storage checks first
            self_cloned.bundle_storage_check(ingress, cancel_token.clone())?;

            // Now check the metadata storage for orphans
            if !cancel_token.is_cancelled() {
                self_cloned.metadata_storage_check(dispatcher, cancel_token)?;
            }

            log::info!("Store consistency check complete");
            Ok::<(), anyhow::Error>(())
        })
        .await?
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
        self.bundle_storage.check_orphans(&mut |storage_name| {
            let r = tokio::runtime::Handle::current().block_on(async {
                // Check if the metadata_storage knows about this bundle
                if self
                    .metadata_storage
                    .confirm_exists(storage_name, None)
                    .await?
                {
                    return Ok::<bool, anyhow::Error>(true);
                }

                // Parse the bundle first
                let data = self.bundle_storage.load(storage_name).await?;
                let Ok((bundle, valid)) = bundle::Bundle::parse((*data).as_ref()) else {
                    // Drop it... garbage
                    return Ok(false);
                };

                // Write to metadata
                let hash = sha2::Sha256::digest((*data).as_ref());
                let metadata = self
                    .metadata_storage
                    .store(
                        bundle::BundleStatus::IngressPending,
                        storage_name,
                        &hash,
                        &bundle,
                    )
                    .await?;

                // Queue the new bundle for ingress processing
                ingress
                    .enqueue_bundle(None, metadata, bundle, valid)
                    .await?;

                // true for keep
                Ok(true)
            })?;

            // Just dumb poll the cancel token now - try to avoid mismatched state again
            Ok((!cancel_token.is_cancelled()).then_some(r))
        })
    }

    pub async fn store(
        &self,
        bundle: &bundle::Bundle,
        data: Vec<u8>,
        status: bundle::BundleStatus,
    ) -> Result<bundle::Metadata, anyhow::Error> {
        // Calculate hash
        let hash = sha2::Sha256::digest(&data);

        // Write to bundle storage
        let storage_name = self.bundle_storage.store(data).await?;

        // Write to metadata store
        match self
            .metadata_storage
            .store(status, &storage_name, &hash, &bundle)
            .await
        {
            Err(e) => {
                // This is just bad, we can't really claim to have received the bundle,
                // so just cleanup and get out
                let _ = self.bundle_storage.remove(&storage_name).await;
                Err(e)
            }
            Ok(r) => Ok(r),
        }
    }

    pub async fn set_bundle_status(
        &self,
        bundle_id: &bundle::BundleId,
        status: bundle::BundleStatus,
    ) -> Result<bundle::BundleStatus, anyhow::Error> {
        self.metadata_storage
            .set_bundle_status(bundle_id, status)
            .await
    }

    pub async fn remove(&self, bundle_id: &bundle::BundleId) -> Result<(), anyhow::Error> {
        todo!()
    }
}
