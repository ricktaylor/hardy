#[cfg(feature = "postgres-storage")]
use core::num::NonZeroU32;
use core::num::NonZeroUsize;
#[cfg(any(feature = "sqlite-storage", feature = "localdisk-storage"))]
use std::path::PathBuf;

#[cfg(feature = "s3-storage")]
use hardy_s3_storage::{MultipartThreshold, PartSize};
#[cfg(feature = "postgres-storage")]
use jiff::SignedDuration;
use serde::{Deserialize, Serialize};

// The `type: "sqlite"` metadata backend schema: absent keys defer to the
// backend's own defaults (the platform cache directory, `metadata.db`).
#[cfg(feature = "sqlite-storage")]
#[derive(Default, Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields, default, rename_all = "kebab-case")]
pub struct SqliteConfig {
    // Directory in which the database file is stored.
    pub db_dir: Option<PathBuf>,

    // Filename of the SQLite database.
    pub db_name: Option<String>,
}

// The `type: "postgres"` metadata backend schema: absent keys defer to the
// backend's own defaults.
#[cfg(feature = "postgres-storage")]
#[derive(Default, Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields, default, rename_all = "kebab-case")]
pub struct PostgresConfig {
    // PostgreSQL connection string (e.g. `postgres://user:pass@host/db`);
    // absent falls back to the `DATABASE_URL` environment variable.
    pub database_url: Option<String>,

    // Maximum number of pooled connections; must be greater than zero.
    // For scale deployments this should be sized to `worker_threads * 2`
    // or higher.
    pub max_connections: Option<NonZeroU32>,

    // Minimum number of idle connections kept alive in the pool.
    pub min_connections: Option<u32>,

    // How long to wait when acquiring a connection before returning an
    // error; a duration string (e.g. `30s`), greater than zero.
    #[serde(with = "positive_duration")]
    pub connect_timeout: Option<SignedDuration>,

    // How long a connection may sit idle before it is closed and removed
    // from the pool; a duration string (e.g. `10m`), greater than zero.
    #[serde(with = "positive_duration")]
    pub idle_timeout: Option<SignedDuration>,

    // Maximum lifetime of a pooled connection; a duration string (e.g.
    // `30m`), greater than zero.
    #[serde(with = "positive_duration")]
    pub max_lifetime: Option<SignedDuration>,

    // Rows fetched per page in keyset-paginated poll queries; must be
    // greater than zero.
    pub poll_page_size: Option<NonZeroU32>,
}

// A pool timing: jiff's ISO 8601 or friendly duration (`30s`, `1h 30m`,
// `PT30S`), refused unless greater than zero, and written back in the
// friendly format.
#[cfg(feature = "postgres-storage")]
mod positive_duration {
    use jiff::{SignedDuration, fmt::serde::duration::friendly::compact};
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _};

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<SignedDuration>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let Some(duration) = Option::<SignedDuration>::deserialize(deserializer)? else {
            return Ok(None);
        };
        if !duration.is_positive() {
            return Err(D::Error::custom("a duration must be greater than zero"));
        }
        Ok(Some(duration))
    }

    pub fn serialize<S>(duration: &Option<SignedDuration>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        compact::optional(duration, serializer)
    }
}

// The `type: "localdisk"` bundle backend schema: absent keys defer to the
// backend's own defaults (the platform cache directory, fsync enabled).
#[cfg(feature = "localdisk-storage")]
#[derive(Default, Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields, default, rename_all = "kebab-case")]
pub struct LocalDiskConfig {
    // Directory where bundle files are stored.
    pub store_dir: Option<PathBuf>,

    // Whether to use fsync for crash-safe atomic writes.
    pub fsync: Option<bool>,
}

// The `type: "s3"` bundle backend schema. AWS credentials are **not**
// configured here: they are resolved via the standard credential chain
// (`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`, an IAM role, or
// `~/.aws/credentials`). Absent optional keys defer to the backend's own
// defaults.
#[cfg(feature = "s3-storage")]
#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct S3Config {
    // S3 bucket name.
    pub bucket: String,

    // Key prefix for all objects stored by hardy (no leading or trailing
    // slash), for when the bucket is shared with other applications.
    #[serde(default)]
    pub prefix: Option<String>,

    // AWS region (e.g. `"us-east-1"`); absent falls back to the
    // `AWS_DEFAULT_REGION` / `AWS_REGION` env vars.
    #[serde(default)]
    pub region: Option<String>,

    // Custom endpoint URL for S3-compatible stores (MinIO, LocalStack,
    // etc.).
    #[serde(default)]
    pub endpoint_url: Option<String>,

    // Force path-style addressing (`http://host/bucket/key` instead of
    // `http://bucket.host/key`), required for MinIO and some
    // S3-compatible stores.
    #[serde(default)]
    pub force_path_style: bool,

    // Bundle size threshold, in bytes, above which multipart upload is
    // used instead of a single `PutObject`; above the S3 5 GiB `PutObject`
    // limit is rejected at parse, and below the part size at startup.
    #[serde(default)]
    pub multipart_threshold: Option<MultipartThreshold>,

    // Size, in bytes, of each part in a multipart upload (all parts
    // except the last); below the S3 protocol minimum of 5 MiB is
    // rejected at parse.
    #[serde(default)]
    pub multipart_part_size: Option<PartSize>,
}

// The `type: "memory"` metadata backend schema: absent keys defer to the
// store's own defaults.
#[derive(Default, Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields, default, rename_all = "kebab-case")]
pub struct MetadataMemConfig {
    // Maximum number of entries (live bundles plus tombstones) held before
    // the store evicts old entries to make room.
    pub max_bundles: Option<NonZeroUsize>,
}

// The `type: "memory"` bundle backend schema: absent keys defer to the
// store's own defaults.
#[derive(Default, Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields, default, rename_all = "kebab-case")]
pub struct BundleMemConfig {
    // Maximum total bytes of bundle data held before least-recently-used
    // bundles are evicted.
    pub capacity: Option<NonZeroUsize>,

    // Minimum number of bundles retained regardless of the byte capacity;
    // must be greater than zero.
    pub min_bundles: Option<NonZeroUsize>,
}

// Metadata storage backend selector (default: `sqlite`).
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum MetadataStorageConfig {
    #[serde(rename = "memory")]
    Memory(MetadataMemConfig),

    #[cfg(feature = "sqlite-storage")]
    #[serde(rename = "sqlite")]
    Sqlite(SqliteConfig),

    #[cfg(feature = "postgres-storage")]
    #[serde(rename = "postgres")]
    Postgres(PostgresConfig),
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
    Memory(BundleMemConfig),

    #[cfg(feature = "localdisk-storage")]
    #[serde(rename = "localdisk")]
    LocalDisk(LocalDiskConfig),

    #[cfg(feature = "s3-storage")]
    #[serde(rename = "s3")]
    S3(S3Config),
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

// Combined storage configuration. The cache knobs are optional: absent
// keys leave the cache's own defaults in force, so no default value is
// restated here.
//
// Defaults are per-field, not container-level: a container-level
// `#[serde(default)]` constructs the whole `Default` up front, and the
// backend selector defaults panic in builds without the default-backend
// features, even when every backend is explicitly configured. Per-field
// defaults fire only for the keys that are actually absent.
#[derive(Default, Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct StorageConfig {
    // LRU bundle cache capacity, in entries; must be greater than zero.
    #[serde(default)]
    pub lru_capacity: Option<NonZeroUsize>,

    // Largest bundle size eligible for caching, in bytes; must be greater
    // than zero.
    #[serde(default)]
    pub max_cached_bundle_size: Option<NonZeroUsize>,

    #[serde(default)]
    pub metadata: MetadataStorageConfig,

    #[serde(default)]
    pub bundle: BundleStorageConfig,
}

impl StorageConfig {
    pub fn uses_cache(&self) -> bool {
        !matches!(&self.bundle, BundleStorageConfig::Memory(_))
    }
}

#[cfg(all(test, feature = "postgres-storage"))]
mod tests {
    use serde_json::error::Category;

    use super::*;

    fn parse(json: &str) -> serde_json::Result<PostgresConfig> {
        serde_json::from_str(json)
    }

    // Friendly and ISO 8601 durations parse, and serialize back in the
    // friendly format.
    #[test]
    fn pool_timings_parse_and_round_trip() {
        let config = parse(
            r#"{"connect-timeout": "30s", "idle-timeout": "PT10M", "max-lifetime": "1h 30m"}"#,
        )
        .unwrap();
        assert_eq!(config.connect_timeout, Some(SignedDuration::from_secs(30)));
        assert_eq!(config.idle_timeout, Some(SignedDuration::from_mins(10)));
        assert_eq!(config.max_lifetime, Some(SignedDuration::from_mins(90)));

        let json = serde_json::to_value(&config).unwrap();
        assert_eq!(json["connect-timeout"], "30s");
        assert_eq!(json["idle-timeout"], "10m");
        assert_eq!(json["max-lifetime"], "1h 30m");
    }

    // An absent or null timing defers to the backend default.
    #[test]
    fn absent_and_null_timings_are_none() {
        assert_eq!(parse("{}").unwrap().connect_timeout, None);
        assert_eq!(
            parse(r#"{"connect-timeout": null}"#)
                .unwrap()
                .connect_timeout,
            None
        );
    }

    // Zero and negative durations parse as SignedDurations, so refusing them
    // is the positivity check's doing; days and malformed text are refused by
    // jiff itself.
    #[test]
    fn non_positive_calendar_and_malformed_timings_are_refused() {
        for text in ["0s", "-5s"] {
            assert!(serde_json::from_str::<SignedDuration>(&format!("\"{text}\"")).is_ok());
        }
        for text in ["0s", "-5s", "1d", "1year", "30"] {
            let err = parse(&format!(r#"{{"connect-timeout": "{text}"}}"#)).unwrap_err();
            assert_eq!(err.classify(), Category::Data, "{text:?}: {err}");
        }
    }
}
