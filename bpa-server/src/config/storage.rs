use core::num::NonZeroUsize;
use std::sync::Arc;

use hardy_bpa::storage::{
    BundleMemStorage, BundleStorage, DEFAULT_LRU_CAPACITY, DEFAULT_MAX_CACHED_BUNDLE_SIZE,
    MetadataMemStorage, MetadataStorage,
};
use serde::{Deserialize, Serialize};

// Metadata storage backend selector (default: `sqlite`).
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum MetadataStorageConfig {
    #[serde(rename = "memory")]
    Memory(hardy_bpa::storage::MetadataMemStorageConfig),

    #[cfg(feature = "sqlite-storage")]
    #[serde(rename = "sqlite")]
    Sqlite(hardy_sqlite_storage::Config),

    #[cfg(feature = "postgres-storage")]
    #[serde(rename = "postgres")]
    Postgres(hardy_postgres_storage::Config),
}

impl Default for MetadataStorageConfig {
    fn default() -> Self {
        cfg_select! {
            feature = "sqlite-storage" => {
                Self::Sqlite(Default::default())
            }
            _ => {
                // The unconfigured default must never silently degrade to
                // the non-persistent memory backend. Explicitly configured
                // backends (including `memory`) remain available in such
                // builds, so this only fires when the default is requested.
                panic!(
                    "no default metadata storage: built without the `sqlite-storage` feature that provides the default backend; configure `storage.metadata` explicitly (e.g. `type: memory`) or rebuild with the feature"
                )
            }
        }
    }
}

// Bundle data storage backend selector (default: `localdisk`).
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum BundleStorageConfig {
    #[serde(rename = "memory")]
    Memory(hardy_bpa::storage::BundleMemStorageConfig),

    #[cfg(feature = "localdisk-storage")]
    #[serde(rename = "localdisk")]
    LocalDisk(hardy_localdisk_storage::Config),

    #[cfg(feature = "s3-storage")]
    #[serde(rename = "s3")]
    S3(hardy_s3_storage::Config),
}

impl Default for BundleStorageConfig {
    fn default() -> Self {
        cfg_select! {
            feature = "localdisk-storage" => {
                Self::LocalDisk(Default::default())
            }
            _ => {
                // The unconfigured default must never silently degrade to
                // the non-persistent memory backend. Explicitly configured
                // backends (including `memory`) remain available in such
                // builds, so this only fires when the default is requested.
                panic!(
                    "no default bundle storage: built without the `localdisk-storage` feature that provides the default backend; configure `storage.bundle` explicitly (e.g. `type: memory`) or rebuild with the feature"
                )
            }
        }
    }
}

// Combined storage configuration.
#[derive(Serialize, Deserialize, Debug)]
#[serde(default, rename_all = "kebab-case")]
pub struct Config {
    pub lru_capacity: NonZeroUsize,
    pub max_cached_bundle_size: NonZeroUsize,
    #[serde(default)]
    pub metadata: MetadataStorageConfig,
    #[serde(default)]
    pub bundle: BundleStorageConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            lru_capacity: DEFAULT_LRU_CAPACITY,
            max_cached_bundle_size: DEFAULT_MAX_CACHED_BUNDLE_SIZE,
            metadata: MetadataStorageConfig::default(),
            bundle: BundleStorageConfig::default(),
        }
    }
}

impl Config {
    pub fn uses_cache(&self) -> bool {
        !matches!(&self.bundle, BundleStorageConfig::Memory(_))
    }

    /// Create the metadata and bundle storage backends from this configuration.
    #[allow(unused_variables)]
    pub async fn build(
        &self,
        upgrade: bool,
    ) -> anyhow::Result<(Arc<dyn MetadataStorage>, Arc<dyn BundleStorage>)> {
        let metadata: Arc<dyn MetadataStorage> = match &self.metadata {
            MetadataStorageConfig::Memory(cfg) => Arc::new(MetadataMemStorage::new(cfg)),

            #[cfg(feature = "sqlite-storage")]
            MetadataStorageConfig::Sqlite(cfg) => hardy_sqlite_storage::new(cfg, upgrade),

            #[cfg(feature = "postgres-storage")]
            MetadataStorageConfig::Postgres(cfg) => {
                hardy_postgres_storage::new(cfg, upgrade).await?
            }
        };

        let bundle: Arc<dyn BundleStorage> = match &self.bundle {
            BundleStorageConfig::Memory(cfg) => Arc::new(BundleMemStorage::new(cfg)),

            #[cfg(feature = "localdisk-storage")]
            BundleStorageConfig::LocalDisk(cfg) => hardy_localdisk_storage::new(cfg, upgrade),

            #[cfg(feature = "s3-storage")]
            BundleStorageConfig::S3(cfg) => hardy_s3_storage::new(cfg).await?,
        };

        Ok((metadata, bundle))
    }
}
