use core::num::NonZeroUsize;
use hardy_async::available_parallelism;
use hardy_bpa::filters::rfc9171;
use hardy_bpa::node_ids::NodeIds;
use hardy_bpv7::eid::Service;
use serde::{Deserialize, Serialize};
use tracing::Level;

use crate::bpa::clas::Cla;
use crate::bpa::grpc;
use crate::bpa::static_routes;
use crate::bpa::storage;
use crate::error::Error;

/// Returns the default config directory, platform-specific:
/// - Linux: /etc/hardy/
/// - macOS: /etc/hardy/
/// - Windows: %ProgramData%\hardy\ (via `directories` crate), or exe directory as fallback
pub(crate) fn default_config_dir() -> std::path::PathBuf {
    #[cfg(unix)]
    return std::path::PathBuf::from("/etc/hardy");

    #[cfg(windows)]
    return directories::BaseDirs::new()
        .map(|dirs| dirs.data_local_dir().join("hardy"))
        .unwrap_or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|p| p.to_path_buf()))
                .unwrap_or_else(|| std::path::PathBuf::from("."))
        });
}

fn default_config_path() -> std::path::PathBuf {
    default_config_dir().join("config.yaml")
}

fn default_log_level() -> Level {
    Level::INFO
}

fn default_poll_channel_depth() -> NonZeroUsize {
    NonZeroUsize::new(16).unwrap()
}

fn default_processing_pool_size() -> NonZeroUsize {
    NonZeroUsize::new(available_parallelism().get() * 4).unwrap()
}

mod log_level_serde {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::str::FromStr;
    use tracing::Level;

    pub fn serialize<S>(level: &Level, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(level.as_str())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Level, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Level::from_str(&s).map_err(serde::de::Error::custom)
    }
}

#[derive(Serialize, Deserialize, Debug, Default)]
#[serde(default, rename_all = "kebab-case")]
pub struct BuiltInServicesConfig {
    /// Echo service: list of service identifiers (int = IPN, string = DTN).
    pub echo: Option<Vec<Service>>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
pub struct Config {
    /// Logging level (default: INFO)
    #[serde(default = "default_log_level", with = "log_level_serde")]
    pub log_level: Level,

    /// Whether to generate and dispatch bundle status reports (default: false)
    #[serde(default)]
    pub status_reports: bool,

    /// Depth of the channel used for polling new bundles (default: 16)
    #[serde(default = "default_poll_channel_depth")]
    pub poll_channel_depth: NonZeroUsize,

    /// Maximum number of concurrent bundle processing tasks (default: 4 * CPU cores)
    #[serde(default = "default_processing_pool_size")]
    pub processing_pool_size: NonZeroUsize,

    /// Endpoint IDs (EIDs) that identify this node (e.g. "ipn:1.0", "dtn://my-node/")
    #[serde(default)]
    pub node_ids: NodeIds,

    /// Static Routes Configuration
    #[serde(default)]
    pub static_routes: Option<static_routes::Config>,

    /// gRPC options
    #[serde(default)]
    pub grpc: Option<grpc::Config>,

    /// Storage configuration (cache + metadata + bundle backends)
    #[serde(default)]
    pub storage: storage::Config,

    #[cfg(feature = "ipn-legacy-filter")]
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
    pub rfc9171_validity: rfc9171::Config,

    /// Built-in application services to register.
    /// Each service key maps to a list of service identifiers to register on.
    /// Integers are IPN service numbers, strings are DTN service names.
    /// Absent key = service disabled.
    #[serde(default)]
    pub built_in_services: BuiltInServicesConfig,

    /// Convergence Layer Adaptors (CLAs)
    #[serde(default)]
    pub clas: Vec<Cla>,
}

impl Config {
    pub fn load(config_file: Option<String>) -> Result<Config, Error> {
        let config_file = config_file
            .or_else(|| std::env::var("HARDY_BPA_SERVER_CONFIG_FILE").ok())
            .unwrap_or_else(|| default_config_path().to_string_lossy().into_owned());

        let source_file = ::config::File::with_name(&config_file);
        let source_env = ::config::Environment::with_prefix("HARDY_BPA_SERVER")
            .prefix_separator("_")
            .separator("__");

        let config = ::config::Config::builder()
            .add_source(source_file)
            .add_source(source_env)
            .build()?
            .try_deserialize()?;

        eprintln!("Loaded configuration from '{config_file}'");
        Ok(config)
    }
}
