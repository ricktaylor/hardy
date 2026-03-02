use super::*;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::str::FromStr;
use tracing::Level;

mod log_level_serde {
    use super::*;

    pub fn serialize<S>(level: &Option<Level>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match level {
            Some(level) => serializer.serialize_some(level.as_str()),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Level>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: Option<String> = Option::deserialize(deserializer)?;
        s.map(|s| Level::from_str(&s).map_err(serde::de::Error::custom))
            .transpose()
    }
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum MetadataStorage {
    #[serde(rename = "memory")]
    Memory(hardy_bpa::storage::metadata_mem::Config),

    #[cfg(feature = "sqlite-storage")]
    #[serde(rename = "sqlite")]
    Sqlite(hardy_sqlite_storage::Config),
    // #[cfg(feature = "postgres-storage")]
    // #[serde(rename = "postgres")]
    // Postgres(PostgresConfig),
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum BundleStorage {
    #[serde(rename = "memory")]
    Memory(hardy_bpa::storage::bundle_mem::Config),

    #[cfg(feature = "localdisk-storage")]
    #[serde(rename = "localdisk")]
    LocalDisk(hardy_localdisk_storage::Config),
    // #[cfg(feature = "s3-storage")]
    // #[serde(rename = "s3")]
    // S3(S3Config),
}

#[derive(Serialize, Deserialize, Debug, Default)]
#[serde(default, rename_all = "kebab-case")]
pub struct StorageConfig {
    /// BPA bundle cache settings (LRU capacity, max cached bundle size).
    #[serde(flatten)]
    pub cache: hardy_bpa::storage::Config,

    /// Metadata storage backend. Uses in-memory if not set.
    pub metadata: Option<MetadataStorage>,

    /// Bundle data storage backend. Uses in-memory if not set.
    pub bundle: Option<BundleStorage>,
}

#[derive(Serialize, Deserialize, Debug, Default)]
#[serde(default, rename_all = "kebab-case")]
pub struct BuiltInServicesConfig {
    /// Echo service: list of service identifiers (int = IPN, string = DTN).
    pub echo: Option<Vec<hardy_bpv7::eid::Service>>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
pub struct Config {
    /// Logging level
    #[serde(default, with = "log_level_serde")]
    pub log_level: Option<Level>,

    /// Static Routes Configuration
    #[serde(default)]
    pub static_routes: Option<static_routes::Config>,

    /// Flattened BPA settings
    #[serde(flatten, default)]
    pub bpa: hardy_bpa::config::Config,

    /// gRPC options
    #[serde(default)]
    pub grpc: Option<grpc::Config>,

    /// Storage configuration (cache + metadata + bundle backends)
    #[serde(default)]
    pub storage: StorageConfig,

    #[serde(default)]
    pub ipn_legacy_nodes: hardy_ipn_legacy_filter::Config,

    /// RFC9171 validity filter configuration.
    ///
    /// Controls the RFC9171 bundle validity checks that are auto-registered
    /// when the `rfc9171-filter` feature is enabled on the BPA.
    ///
    /// Set individual fields to `false` to disable specific checks:
    /// - `primary_block_integrity`: Require CRC or BIB on primary block
    /// - `bundle_age_required`: Require Bundle Age when creation time has no clock
    #[serde(default)]
    pub rfc9171_validity: hardy_bpa::filters::rfc9171::Config,

    /// Built-in application services to register.
    /// Each service key maps to a list of service identifiers to register on.
    /// Integers are IPN service numbers, strings are DTN service names.
    /// Absent key = service disabled.
    #[serde(default)]
    pub built_in_services: BuiltInServicesConfig,

    /// Convergence Layer Adaptors (CLAs)
    #[serde(default)]
    pub clas: Vec<clas::Cla>,
}

pub fn config_dir() -> std::path::PathBuf {
    directories::ProjectDirs::from("dtn", "Hardy", env!("CARGO_PKG_NAME")).map_or_else(
        || {
            #[cfg(all(target_os = "linux", not(feature = "packaged-installation")))]
            return std::path::Path::new("/etc/opt").join(env!("CARGO_PKG_NAME"));

            #[cfg(all(
                unix,
                not(all(target_os = "linux", not(feature = "packaged-installation")))
            ))]
            return std::path::Path::new("/etc").join(env!("CARGO_PKG_NAME"));

            #[cfg(windows)]
            return std::env::current_exe()
                .trace_expect("Failed to get current executable path")
                .join(env!("CARGO_PKG_NAME"));

            #[cfg(not(any(unix, windows)))]
            compile_error!("No idea how to determine default config directory for target platform");
        },
        |proj_dirs| {
            proj_dirs.config_local_dir().to_path_buf()
            // Lin: /home/alice/.config/barapp
            // Win: C:\Users\Alice\AppData\Roaming\Foo Corp\Bar App\config
            // Mac: /Users/Alice/Library/Application Support/com.Foo-Corp.Bar-App
        },
    )
}

pub fn load(cli: &cli::Args) -> Config {
    let mut b = ::config::Config::builder();

    if let Some(source) = &cli.config_file {
        eprintln!("Using configuration file '{source}' specified on command line");
        b = b.add_source(::config::File::with_name(source))
    } else if let Ok(source) = std::env::var("HARDY_BPA_SERVER_CONFIG_FILE") {
        eprintln!(
            "Using configuration file '{source}' specified by HARDY_BPA_SERVER_CONFIG_FILE environment variable"
        );
        b = b.add_source(::config::File::with_name(&source))
    } else {
        let path = config_dir().join(format!("{}.yaml", env!("CARGO_PKG_NAME")));
        eprintln!("Using configuration file '{}'", path.display());
        b = b.add_source(::config::File::from(path).required(false))
    }

    b = b.add_source(::config::Environment::with_prefix("HARDY_BPA_SERVER"));

    b.build()
        .expect("Failed to read configuration")
        .try_deserialize()
        .expect("Failed to parse configuration")
}
