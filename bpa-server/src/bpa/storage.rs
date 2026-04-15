use serde::{Deserialize, Serialize};
use std::sync::Arc;

// Metadata storage backend selector (default: `memory`).
//
// The `type` field in the config file selects the variant.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum MetadataStorage {
    // In-memory metadata store (non-persistent, for development/testing).
    #[serde(rename = "memory")]
    Memory(hardy_bpa::storage::metadata_mem::Config),

    // SQLite-backed metadata store. Requires the `sqlite-storage` feature.
    #[cfg(feature = "sqlite-storage")]
    #[serde(rename = "sqlite")]
    Sqlite(hardy_sqlite_storage::Config),

    // PostgreSQL-backed metadata store. Requires the `postgres-storage` feature.
    #[cfg(feature = "postgres-storage")]
    #[serde(rename = "postgres")]
    Postgres(hardy_postgres_storage::Config),
}

impl Default for MetadataStorage {
    fn default() -> Self {
        Self::Memory(Default::default())
    }
}

// Bundle data storage backend selector (default: `memory`).
//
// The `type` field in the config file selects the variant.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum BundleStorage {
    // In-memory bundle store (non-persistent, for development/testing).
    #[serde(rename = "memory")]
    Memory(hardy_bpa::storage::bundle_mem::Config),

    // Local-disk bundle store. Requires the `localdisk-storage` feature.
    #[cfg(feature = "localdisk-storage")]
    #[serde(rename = "localdisk")]
    LocalDisk(hardy_localdisk_storage::Config),

    // S3-compatible object store. Requires the `s3-storage` feature.
    #[cfg(feature = "s3-storage")]
    #[serde(rename = "s3")]
    S3(hardy_s3_storage::Config),
}

impl Default for BundleStorage {
    fn default() -> Self {
        Self::Memory(Default::default())
    }
}

// Combined storage configuration covering the LRU cache and both backends.
#[derive(Serialize, Deserialize, Debug)]
#[serde(default, rename_all = "kebab-case")]
pub struct Config {
    // LRU capacity for the bundle cache.
    pub lru_capacity: core::num::NonZeroUsize,
    // Max size of a single bundle to keep in the LRU cache (bytes).
    pub max_cached_bundle_size: core::num::NonZeroUsize,
    // Metadata storage backend.
    #[serde(default)]
    pub metadata: MetadataStorage,
    // Bundle data storage backend.
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
    // Returns `true` if the bundle backend benefits from an LRU cache
    // (i.e. it is not purely in-memory).
    pub fn uses_cache(&self) -> bool {
        !matches!(&self.bundle, BundleStorage::Memory(_))
    }
}

// Initialised storage backends ready to be handed to the BPA builder.
pub struct Storage {
    // The metadata storage backend instance.
    pub metadata: Arc<dyn hardy_bpa::storage::MetadataStorage>,
    // The bundle data storage backend instance.
    pub bundle: Arc<dyn hardy_bpa::storage::BundleStorage>,
}

impl Storage {
    // Create the metadata and bundle storage backends from configuration.
    //
    // If `upgrade` is true, backend-specific schema migrations are applied.
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
