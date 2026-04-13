use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum MetadataStorage {
    #[serde(rename = "memory")]
    Memory(hardy_bpa::storage::metadata_mem::Config),

    #[cfg(feature = "sqlite-storage")]
    #[serde(rename = "sqlite")]
    Sqlite(hardy_sqlite_storage::Config),

    #[cfg(feature = "postgres-storage")]
    #[serde(rename = "postgres")]
    Postgres(hardy_postgres_storage::Config),
}

impl Default for MetadataStorage {
    fn default() -> Self {
        Self::Memory(Default::default())
    }
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum BundleStorage {
    #[serde(rename = "memory")]
    Memory(hardy_bpa::storage::bundle_mem::Config),

    #[cfg(feature = "localdisk-storage")]
    #[serde(rename = "localdisk")]
    LocalDisk(hardy_localdisk_storage::Config),

    #[cfg(feature = "s3-storage")]
    #[serde(rename = "s3")]
    S3(hardy_s3_storage::Config),
}

impl Default for BundleStorage {
    fn default() -> Self {
        Self::Memory(Default::default())
    }
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(default, rename_all = "kebab-case")]
pub struct Config {
    /// LRU capacity for the bundle cache.
    pub lru_capacity: core::num::NonZeroUsize,
    /// Max size of a single bundle to keep in the LRU cache (bytes).
    pub max_cached_bundle_size: core::num::NonZeroUsize,
    /// Metadata storage backend.
    #[serde(default)]
    pub metadata: MetadataStorage,
    /// Bundle data storage backend.
    #[serde(default)]
    pub bundle: BundleStorage,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            lru_capacity: hardy_bpa::storage::DEFAULT_LRU_CAPACITY,
            max_cached_bundle_size: hardy_bpa::storage::DEFAULT_MAX_CACHED_BUNDLE_SIZE,
            metadata: MetadataStorage::default(),
            bundle: BundleStorage::default(),
        }
    }
}

impl Config {
    pub fn uses_cache(&self) -> bool {
        !matches!(&self.bundle, BundleStorage::Memory(_))
    }
}

pub struct Storage {
    pub metadata: Arc<dyn hardy_bpa::storage::MetadataStorage>,
    pub bundle: Arc<dyn hardy_bpa::storage::BundleStorage>,
}

impl Storage {
    #[allow(unused_variables)]
    pub async fn try_new(config: &Config, upgrade: bool) -> anyhow::Result<Self> {
        let metadata: Arc<dyn hardy_bpa::storage::MetadataStorage> = match &config.metadata {
            MetadataStorage::Memory(cfg) => Arc::new(
                hardy_bpa::storage::metadata_mem::MetadataMemStorage::new(cfg),
            ),

            #[cfg(feature = "sqlite-storage")]
            MetadataStorage::Sqlite(cfg) => hardy_sqlite_storage::new(cfg, upgrade),

            #[cfg(feature = "postgres-storage")]
            MetadataStorage::Postgres(cfg) => hardy_postgres_storage::new(cfg, upgrade).await?,
        };

        let bundle: Arc<dyn hardy_bpa::storage::BundleStorage> = match &config.bundle {
            BundleStorage::Memory(cfg) => {
                Arc::new(hardy_bpa::storage::bundle_mem::BundleMemStorage::new(cfg))
            }

            #[cfg(feature = "localdisk-storage")]
            BundleStorage::LocalDisk(cfg) => hardy_localdisk_storage::new(cfg, upgrade),

            #[cfg(feature = "s3-storage")]
            BundleStorage::S3(cfg) => hardy_s3_storage::new(cfg).await?,
        };

        Ok(Self { metadata, bundle })
    }
}
