use core::num::NonZeroUsize;
use std::collections::HashMap;
use std::path::PathBuf;

use hardy_async::available_parallelism;
use hardy_bpa::filter::rfc9171;
use hardy_bpa::node_ids::NodeIds;
use hardy_bpv7::eid::Service;
use serde::{Deserialize, Serialize};
use tracing::Level;

pub mod cla;
pub mod policy;
pub mod storage;

use crate::error::Error;
use crate::static_routes;

// Returns the default config directory, platform-specific:
// - Linux: /etc/hardy/
// - macOS: /etc/hardy/
// - Windows: %ProgramData%\hardy\ (via `directories` crate), or exe directory as fallback
pub fn default_config_dir() -> std::path::PathBuf {
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
    default_config_dir().join("bpa")
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

// Configuration for built-in application services.
#[derive(Serialize, Deserialize, Debug, Default)]
#[serde(default, rename_all = "kebab-case")]
pub struct BuiltInServicesConfig {
    // Echo service: list of service identifiers (int = IPN, string = DTN).
    // Absent = service disabled.
    pub echo: Option<Vec<Service>>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
pub struct Config {
    // Logging level (default: INFO)
    #[serde(default = "default_log_level", with = "log_level_serde")]
    pub log_level: Level,

    // Whether to generate and dispatch bundle status reports (default: false)
    #[serde(default)]
    pub status_reports: bool,

    // Depth of the channel used for polling new bundles (default: 16)
    #[serde(default = "default_poll_channel_depth")]
    pub poll_channel_depth: NonZeroUsize,

    // Maximum number of concurrent bundle processing tasks (default: 4 * CPU cores)
    #[serde(default = "default_processing_pool_size")]
    pub processing_pool_size: NonZeroUsize,

    // Endpoint IDs (EIDs) that identify this node (e.g. "ipn:1.0", "dtn://my-node/")
    #[serde(default)]
    pub node_ids: NodeIds,

    // The routing priority of services (default 1)
    #[serde(default)]
    pub service_priority: Option<u32>,

    // Static Routes Configuration
    #[serde(default)]
    pub static_routes: Option<static_routes::Config>,

    // gRPC options
    #[serde(default)]
    #[cfg(feature = "grpc")]
    pub grpc: Option<hardy_proto::server::Config>,

    // Storage configuration (cache + metadata + bundle backends)
    #[serde(default)]
    pub storage: storage::Config,

    // IPN legacy node patterns for the egress rewriting filter.
    #[cfg(feature = "ipn-legacy-filter")]
    #[serde(default)]
    pub ipn_legacy_nodes: hardy_ipn_legacy_filter::Config,

    // RFC9171 validity filter configuration.
    //
    // Controls the RFC9171 bundle validity checks that are auto-registered
    // when the `rfc9171-filter` feature is enabled on the BPA.
    //
    // Set individual fields to `false` to disable specific checks:
    // - `primary_block_integrity`: Require CRC or BIB on primary block
    // - `bundle_age_required`: Require Bundle Age when creation time has no clock
    #[serde(default)]
    pub rfc9171_validity: rfc9171::Config,

    // Built-in application services to register.
    // Each service key maps to a list of service identifiers to register on.
    // Integers are IPN service numbers, strings are DTN service names.
    // Absent key = service disabled.
    #[serde(default)]
    pub built_in_services: BuiltInServicesConfig,

    /// Named egress policies, referenced by CLAs
    #[serde(default)]
    pub policies: HashMap<String, policy::EgressPolicyConfig>,

    /// Convergence Layer Adaptors (CLAs)
    #[serde(default)]
    pub clas: Vec<cla::Config>,
}

impl Config {
    // Load the BPA server configuration.
    //
    // Resolution order: explicit `config_file` path, then `HARDY_BPA_SERVER_CONFIG_FILE`
    // env var, then the platform default (`/etc/hardy/bpa` on Unix).
    // Environment variables prefixed with `HARDY_BPA_SERVER_` override file values.
    pub fn load(config_file: Option<PathBuf>) -> Result<Config, Error> {
        let config_file = config_file
            .or_else(|| {
                std::env::var("HARDY_BPA_SERVER_CONFIG_FILE")
                    .ok()
                    .map(PathBuf::from)
            })
            .unwrap_or_else(default_config_path);

        let source_file = ::config::File::with_name(&config_file.to_string_lossy());
        let source_env = ::config::Environment::with_prefix("HARDY_BPA_SERVER")
            .prefix_separator("_")
            .separator("__")
            .convert_case(::config::Case::Kebab)
            .try_parsing(true);

        let config = ::config::Config::builder()
            .add_source(source_file)
            .add_source(source_env)
            .build()?
            .try_deserialize()?;

        eprintln!("Loaded configuration from '{}'", config_file.display());
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // Helper: write a config file and load it.
    fn write_and_load(name: &str, content: &str) -> Config {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        Config::load(Some(path)).unwrap()
    }

    // Empty config file produces sensible defaults.
    #[test]
    #[serial]
    fn empty_config_has_defaults() {
        let config = write_and_load("empty.yaml", "");
        assert_eq!(config.log_level, Level::INFO);
        assert!(!config.status_reports);
        assert_eq!(config.poll_channel_depth.get(), 16);
        #[cfg(feature = "grpc")]
        assert!(config.grpc.is_none());
        assert!(config.static_routes.is_none());
        assert!(config.clas.is_empty());
    }

    // YAML config file overrides defaults.
    #[test]
    #[serial]
    fn yaml_overrides_defaults() {
        let config = write_and_load(
            "test.yaml",
            r#"
log-level: debug
status-reports: true
poll-channel-depth: 32
node-ids:
  - "ipn:42.0"
"#,
        );
        assert_eq!(config.log_level, Level::DEBUG);
        assert!(config.status_reports);
        assert_eq!(config.poll_channel_depth.get(), 32);
    }

    // TOML config file works identically to YAML.
    #[test]
    #[serial]
    fn toml_config() {
        let config = write_and_load(
            "test.toml",
            r#"
log-level = "warn"
status-reports = true
poll-channel-depth = 64
"#,
        );
        assert_eq!(config.log_level, Level::WARN);
        assert!(config.status_reports);
        assert_eq!(config.poll_channel_depth.get(), 64);
    }

    // JSON config file works identically to YAML.
    #[test]
    #[serial]
    fn json_config() {
        let config = write_and_load(
            "test.json",
            r#"{
    "log-level": "error",
    "status-reports": true
}"#,
        );
        assert_eq!(config.log_level, Level::ERROR);
        assert!(config.status_reports);
    }

    // Environment variables override config file values.
    #[test]
    #[serial]
    fn env_overrides_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.yaml");
        std::fs::write(&path, "log-level: info\nstatus-reports: false\n").unwrap();

        unsafe { std::env::set_var("HARDY_BPA_SERVER_LOG_LEVEL", "debug") };
        unsafe { std::env::set_var("HARDY_BPA_SERVER_STATUS_REPORTS", "true") };
        let config = Config::load(Some(path)).unwrap();
        unsafe { std::env::remove_var("HARDY_BPA_SERVER_LOG_LEVEL") };
        unsafe { std::env::remove_var("HARDY_BPA_SERVER_STATUS_REPORTS") };

        assert_eq!(
            config.log_level,
            Level::DEBUG,
            "env var should override log level"
        );
        assert!(
            config.status_reports,
            "env var should override status-reports"
        );
    }

    // Nested env vars with __ separator override nested config fields.
    #[test]
    #[serial]
    fn env_overrides_nested_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.yaml");
        std::fs::write(
            &path,
            "storage:\n  metadata:\n    type: memory\n  bundle:\n    type: memory\n",
        )
        .unwrap();

        unsafe { std::env::set_var("HARDY_BPA_SERVER_POLL_CHANNEL_DEPTH", "128") };
        let config = Config::load(Some(path)).unwrap();
        unsafe { std::env::remove_var("HARDY_BPA_SERVER_POLL_CHANNEL_DEPTH") };

        assert_eq!(config.poll_channel_depth.get(), 128);
    }

    // Missing config file returns an error.
    #[test]
    #[serial]
    fn missing_config_file_errors() {
        let result = Config::load(Some(PathBuf::from("/nonexistent/path/config")));
        assert!(result.is_err());
    }

    // Invalid log level in config file returns an error.
    #[test]
    #[serial]
    fn invalid_log_level_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        std::fs::write(&path, "log-level: banana\n").unwrap();
        let result = Config::load(Some(path));
        assert!(result.is_err());
    }

    // Zero value for NonZeroUsize fields is rejected.
    #[test]
    #[serial]
    fn zero_pool_size_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        std::fs::write(&path, "processing-pool-size: 0\n").unwrap();
        let result = Config::load(Some(path));
        assert!(result.is_err());
    }

    // Zero value for poll-channel-depth is rejected.
    #[test]
    #[serial]
    fn zero_poll_channel_depth_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        std::fs::write(&path, "poll-channel-depth: 0\n").unwrap();
        let result = Config::load(Some(path));
        assert!(result.is_err());
    }

    // Negative values for unsigned fields are rejected.
    #[test]
    #[serial]
    fn negative_value_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        std::fs::write(&path, "poll-channel-depth: -1\n").unwrap();
        let result = Config::load(Some(path));
        assert!(result.is_err());
    }

    // CLA list with one tcpclv4 entry parses correctly.
    #[test]
    #[serial]
    fn cla_list_parsing() {
        let config = write_and_load(
            "cla.yaml",
            r#"
clas:
  - name: "tcp-cla-1"
    type: tcpclv4
    address: "[::]:4556"
    segment-mru: 8192
  - name: "tcp-cla-2"
    type: tcpclv4
    address: "[::]:4557"
"#,
        );
        assert_eq!(config.clas.len(), 2);
        assert_eq!(config.clas[0].name, "tcp-cla-1");
        assert_eq!(config.clas[1].name, "tcp-cla-2");
    }

    // Empty CLA list is valid.
    #[test]
    #[serial]
    fn empty_cla_list() {
        let config = write_and_load("empty_cla.yaml", "clas: []\n");
        assert!(config.clas.is_empty());
    }

    // Built-in echo service parses integer and string identifiers.
    #[test]
    #[serial]
    fn echo_service_parsing() {
        let config = write_and_load(
            "echo.yaml",
            r#"
built-in-services:
  echo:
    - 7
    - echo
"#,
        );
        let echo = config.built_in_services.echo.unwrap();
        assert_eq!(echo.len(), 2);
    }

    // Storage type selection parses correctly.
    #[test]
    #[serial]
    fn storage_memory_config() {
        let config = write_and_load(
            "storage.yaml",
            r#"
storage:
  metadata:
    type: memory
  bundle:
    type: memory
"#,
        );
        // Should load without error — memory is always available.
        assert_eq!(config.log_level, Level::INFO);
    }

    // Malformed YAML returns an error.
    #[test]
    #[serial]
    fn malformed_yaml_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        std::fs::write(&path, "clas:\n  - name: [broken\n").unwrap();
        let result = Config::load(Some(path));
        assert!(result.is_err());
    }

    // Malformed TOML returns an error.
    #[test]
    #[serial]
    fn malformed_toml_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "log-level = \n").unwrap();
        let result = Config::load(Some(path));
        assert!(result.is_err());
    }

    // Malformed JSON returns an error.
    #[test]
    #[serial]
    fn malformed_json_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "{\"log-level\":}").unwrap();
        let result = Config::load(Some(path));
        assert!(result.is_err());
    }

    // Unknown fields are silently ignored (config-rs behavior).
    #[test]
    #[serial]
    fn unknown_fields_ignored() {
        let config = write_and_load(
            "extra.yaml",
            r#"
log-level: warn
this-field-does-not-exist: 42
another-unknown:
  nested: true
"#,
        );
        assert_eq!(config.log_level, Level::WARN);
    }

    // Node IDs can be a single string.
    #[test]
    #[serial]
    fn single_node_id() {
        // Parsing succeeds without error.
        write_and_load("node.yaml", "node-ids: \"ipn:1.0\"\n");
    }

    // Node IDs can be a list with both schemes.
    #[test]
    #[serial]
    fn multiple_node_ids() {
        // Parsing succeeds without error.
        write_and_load(
            "nodes.yaml",
            r#"
node-ids:
  - "ipn:1.0"
  - "dtn://my-node/"
"#,
        );
    }
}
