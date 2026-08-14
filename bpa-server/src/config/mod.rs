use core::num::NonZeroUsize;
use std::{collections::HashMap, path::PathBuf};

use hardy_async::watcher::WatchMode;
use hardy_bpa::node_ids::NodeIds;
use hardy_bpv7::eid::Service;
use serde::{Deserialize, Serialize};
use tracing::Level;

use crate::error::Error;

pub mod bpsec;
pub mod cla;
pub mod storage;

// Returns the default config directory, platform-specific:
// - Linux: /etc/hardy/
// - macOS: /etc/hardy/
// - Windows: %ProgramData%\hardy\ (via `directories` crate), or exe directory as fallback
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
    default_config_dir().join("bpa")
}

fn default_log_level() -> Level {
    Level::INFO
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

// A positive duration, written as a humantime string (e.g. `30s`, `10m`,
// `1h 30m`).
#[cfg(feature = "postgres-storage")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NonZeroDuration(std::time::Duration);

#[cfg(feature = "postgres-storage")]
impl NonZeroDuration {
    pub fn new(duration: std::time::Duration) -> Option<Self> {
        (!duration.is_zero()).then_some(Self(duration))
    }

    pub fn get(&self) -> std::time::Duration {
        self.0
    }
}

#[cfg(feature = "postgres-storage")]
impl<'de> Deserialize<'de> for NonZeroDuration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        let duration = humantime::parse_duration(&text)
            .map_err(|e| serde::de::Error::custom(format_args!("invalid duration: {e}")))?;
        NonZeroDuration::new(duration)
            .ok_or_else(|| serde::de::Error::custom("a duration must be greater than zero"))
    }
}

#[cfg(feature = "postgres-storage")]
impl Serialize for NonZeroDuration {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.collect_str(&humantime::format_duration(self.0))
    }
}

/// File watch configuration for config files.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WatchConfig {
    /// Watching disabled.
    None,
    /// OS-native events (inotify/kqueue).
    #[default]
    Native,
    /// Periodic polling (~2s). Works in Docker.
    Poll,
}

impl From<WatchConfig> for Option<WatchMode> {
    fn from(config: WatchConfig) -> Self {
        match config {
            WatchConfig::None => Option::None,
            WatchConfig::Native => Some(WatchMode::Native),
            WatchConfig::Poll => Some(WatchMode::Poll),
        }
    }
}

// Configuration for built-in application services.
// The RFC9171 validity checks: absent keys defer to the filter's own
// defaults (all checks enabled).
#[derive(Serialize, Deserialize, Debug, Default)]
#[serde(deny_unknown_fields, default, rename_all = "kebab-case")]
pub struct Rfc9171ValidityConfig {
    // Require CRC or BIB on the primary block; disable for
    // interoperability with implementations that don't add a CRC.
    pub primary_block_integrity: Option<bool>,

    // Require a Bundle Age block when the creation time has no clock.
    pub bundle_age_required: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", content = "config")]
pub enum EgressPolicyConfig {
    #[serde(other)]
    Unknown,
}

// Absent keys defer to the agent's own defaults.
#[derive(Clone, Default, Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields, default, rename_all = "kebab-case")]
pub struct StaticRoutesConfig {
    /// Path to the routes file (default: the `static_routes` file in the
    /// platform config directory, e.g. `/etc/hardy/static_routes`).
    pub routes_file: Option<PathBuf>,
    /// Default route priority when not specified per-route (default: `100`).
    pub priority: Option<u32>,
    /// Watch the routes file for changes and reload automatically.
    /// Values: "native" (default), "poll" (works in Docker), "none" to disable.
    pub watch: WatchConfig,
    /// Protocol identifier used when registering with the BPA (default: `"static_routes"`).
    pub protocol_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Default)]
#[serde(deny_unknown_fields, default, rename_all = "kebab-case")]
pub struct BuiltInServicesConfig {
    // Echo service: list of service identifiers (int = IPN, string = DTN).
    // Absent = service disabled.
    pub echo: Option<Vec<Service>>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    // Logging level (default: INFO)
    #[serde(default = "default_log_level", with = "log_level_serde")]
    pub log_level: Level,

    // Whether to generate and dispatch bundle status reports; absent
    // defers to the BPA default.
    #[serde(default)]
    pub status_reports: Option<bool>,

    // Depth of the channel used for polling new bundles; absent defers to
    // the BPA default.
    #[serde(default)]
    pub poll_channel_depth: Option<NonZeroUsize>,

    // Maximum number of concurrent bundle processing tasks; absent defers
    // to the BPA default.
    #[serde(default)]
    pub processing_pool_size: Option<NonZeroUsize>,

    // Endpoint IDs (EIDs) that identify this node (e.g. "ipn:1.0", "dtn://my-node/")
    #[serde(default)]
    pub node_ids: NodeIds,

    // The routing priority of services (default 1)
    #[serde(default)]
    pub service_priority: Option<u32>,

    // Static Routes Configuration
    #[serde(default)]
    pub static_routes: Option<StaticRoutesConfig>,

    // gRPC options
    #[serde(default)]
    #[cfg(feature = "grpc")]
    pub grpc: Option<hardy_proto::server::Config>,

    // Storage configuration (cache + metadata + bundle backends)
    #[serde(default)]
    pub storage: storage::StorageConfig,

    // IPN legacy node patterns for the egress rewriting filter.
    #[cfg(feature = "ipn-legacy-filter")]
    #[serde(default)]
    pub ipn_legacy_nodes: hardy_ipn_legacy_filter::Config,

    // RFC9171 bundle validity checks.
    #[serde(default)]
    pub rfc9171_validity: Rfc9171ValidityConfig,

    // Built-in application services to register.
    // Each service key maps to a list of service identifiers to register on.
    // Integers are IPN service numbers, strings are DTN service names.
    // Absent key = service disabled.
    #[serde(default)]
    pub built_in_services: BuiltInServicesConfig,

    // BPSec configuration: keys and key bindings (RFC 9172).
    // Absent = no keys loaded, BPSec blocks will fail with NoKey.
    #[serde(default)]
    pub bpsec: Option<bpsec::BPSecConfig>,

    /// Named egress policies, referenced by CLAs
    #[serde(default)]
    pub policies: HashMap<String, EgressPolicyConfig>,

    /// Convergence Layer Adaptors (CLAs)
    #[serde(default)]
    pub clas: Vec<cla::ClaConfig>,
}

impl Config {
    // Load the BPA server configuration.
    //
    // Resolution order: explicit `config_file` path, then `HARDY_BPA_SERVER_CONFIG_FILE`
    // env var, then the platform default (`/etc/hardy/bpa` on Unix).
    // Environment variables prefixed with `HARDY_BPA_SERVER_` override file values.
    pub fn load(config_file: Option<PathBuf>) -> Result<Config, Error> {
        const CONFIG_FILE_VAR: &str = "HARDY_BPA_SERVER_CONFIG_FILE";

        let config_file = config_file
            .or_else(|| std::env::var(CONFIG_FILE_VAR).ok().map(PathBuf::from))
            .unwrap_or_else(default_config_path);

        let source_file = ::config::File::with_name(&config_file.to_string_lossy());
        // `CONFIG_FILE_VAR` is consumed above to locate the file; exclude
        // it from the override source so the strict schema does not refuse
        // it as an unknown `config-file` key. Iterating the OS environment
        // skips non-Unicode variables instead of panicking on them.
        let overrides: ::config::Map<String, String> = std::env::vars_os()
            .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
            .filter(|(key, _)| key != CONFIG_FILE_VAR)
            .collect();
        let source_env = ::config::Environment::with_prefix("HARDY_BPA_SERVER")
            .prefix_separator("_")
            .separator("__")
            .convert_case(::config::Case::Kebab)
            .try_parsing(true)
            .source(Some(overrides));

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

    // Empty config file produces sensible defaults. In a build without the
    // default storage features, relying on the unconfigured storage default
    // panics instead of silently degrading to the memory backend.
    #[test]
    #[serial]
    #[cfg_attr(
        not(all(feature = "sqlite-storage", feature = "localdisk-storage")),
        should_panic(expected = "built without the")
    )]
    fn empty_config_has_defaults() {
        let config = write_and_load("empty.yaml", "");
        assert_eq!(config.log_level, Level::INFO);
        // Absent knobs stay unset, deferring to the BPA builder defaults.
        assert!(config.status_reports.is_none());
        assert!(config.poll_channel_depth.is_none());
        assert!(config.processing_pool_size.is_none());
        #[cfg(feature = "grpc")]
        assert!(config.grpc.is_none());
        assert!(config.static_routes.is_none());
        assert!(config.clas.is_empty());

        // Unconfigured storage defaults to the persistent backends
        #[cfg(feature = "sqlite-storage")]
        assert!(matches!(
            config.storage.metadata,
            storage::MetadataStorageConfig::Sqlite(_)
        ));
        #[cfg(feature = "localdisk-storage")]
        assert!(matches!(
            config.storage.bundle,
            storage::BundleStorageConfig::LocalDisk(_)
        ));
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
        assert_eq!(config.status_reports, Some(true));
        assert_eq!(config.poll_channel_depth.map(|v| v.get()), Some(32));
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
        assert_eq!(config.status_reports, Some(true));
        assert_eq!(config.poll_channel_depth.map(|v| v.get()), Some(64));
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
        assert_eq!(config.status_reports, Some(true));
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
        assert_eq!(
            config.status_reports,
            Some(true),
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

        assert_eq!(config.poll_channel_depth.map(|v| v.get()), Some(128));
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
    listeners: ["[::]:4556"]
    segment-mru: 8192
  - name: "tcp-cla-2"
    type: tcpclv4
    listeners: ["[::]:4557"]
"#,
        );
        assert_eq!(config.clas.len(), 2);
        assert_eq!(config.clas[0].name, "tcp-cla-1");
        assert_eq!(config.clas[1].name, "tcp-cla-2");
    }

    // A full tls block, including client-auth and the private-key-file
    // alias, parses through the tagged clas entry.
    #[test]
    #[serial]
    #[cfg(feature = "tcpclv4")]
    fn cla_tls_block_parsing() {
        let config = write_and_load(
            "cla_tls.yaml",
            r#"
clas:
  - name: "tcp-tls"
    type: tcpclv4
    tls:
      required: true
      ca-certs: "/etc/hardy/ca"
      identity:
        cert-file: "/etc/hardy/certs/server.crt"
        private-key-file: "/etc/hardy/private/server.key"
      client-auth: "required"
"#,
        );
        let cla::ClaType::TcpClv4(tcpcl) = &config.clas[0].cla_type else {
            panic!("expected a tcpclv4 CLA entry");
        };
        let tls = tcpcl.tls.as_ref().unwrap();
        assert!(tls.required);
        assert_eq!(
            tls.identity.as_ref().unwrap().key_file,
            std::path::PathBuf::from("/etc/hardy/private/server.key")
        );
        assert_eq!(tls.client_auth, cla::ClientAuth::Required);
        assert_eq!(
            tls.ca_certs.as_deref(),
            Some(std::path::Path::new("/etc/hardy/ca"))
        );
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
        // Explicitly selecting memory must override the persistent defaults
        assert!(matches!(
            config.storage.metadata,
            storage::MetadataStorageConfig::Memory(_)
        ));
        assert!(matches!(
            config.storage.bundle,
            storage::BundleStorageConfig::Memory(_)
        ));
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

    // `HARDY_BPA_SERVER_CONFIG_FILE` is the loader's own interface,
    // consumed to locate the file; the strict schema must not refuse it as
    // an unknown `config-file` key.
    #[test]
    #[serial]
    fn config_file_env_var_is_not_a_schema_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("via-env.yaml");
        std::fs::write(&path, "log-level: warn\n").unwrap();

        unsafe { std::env::set_var("HARDY_BPA_SERVER_CONFIG_FILE", &path) };
        let result = Config::load(None);
        unsafe { std::env::remove_var("HARDY_BPA_SERVER_CONFIG_FILE") };

        let config = result.expect("the loader's own env var must not be a schema error");
        assert_eq!(config.log_level, Level::WARN);
    }

    // Unknown keys are refused with the known keys listed, at the top
    // level and inside each section, so a removed key or a typo cannot
    // silently leave a default in force.
    #[test]
    #[serial]
    fn unknown_fields_are_refused() {
        let dir = tempfile::tempdir().unwrap();

        let path = dir.path().join("extra.yaml");
        std::fs::write(&path, "log-level: warn\nthis-field-does-not-exist: 42\n").unwrap();
        let Err(err) = Config::load(Some(path)) else {
            panic!("expected a parse error");
        };
        let err = err.to_string();
        assert!(err.contains("this-field-does-not-exist"), "{err}");

        // Sections are strict too: a typo'd storage knob is refused, not
        // silently left at the default.
        let path = dir.path().join("nested.yaml");
        std::fs::write(&path, "storage:\n  lru-capactiy: 16\n").unwrap();
        let Err(err) = Config::load(Some(path)) else {
            panic!("expected a parse error");
        };
        let err = err.to_string();
        assert!(err.contains("lru-capactiy"), "{err}");
    }

    // The shipped example config parses under the strict schema, so its
    // keys cannot drift from the real ones.
    #[test]
    #[serial]
    #[cfg(all(
        feature = "grpc",
        feature = "postgres-storage",
        feature = "s3-storage",
        feature = "tcpclv4"
    ))]
    fn example_config_parses() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config.yaml");
        Config::load(Some(path)).expect("the shipped config.yaml must parse");
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

    // BPSec config parses from YAML.
    #[test]
    #[serial]
    fn bpsec_config_parses() {
        let dir = tempfile::tempdir().unwrap();

        let keys_path = dir.path().join("keys.jwks");
        std::fs::write(
            &keys_path,
            r#"{ "keys": [{ "kid": "k", "kty": "oct", "k": "AAAA", "key_ops": ["verify"] }] }"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&keys_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        let config_path = dir.path().join("config.yaml");
        std::fs::write(
            &config_path,
            format!(
                "bpsec:\n  keys-file: \"{}\"\n  bindings:\n    - match: \"ipn:*.*\"\n      keys: [\"k\"]\n",
                keys_path.display()
            ),
        )
        .unwrap();

        let config = Config::load(Some(config_path)).unwrap();
        assert!(config.bpsec.is_some());
        assert_eq!(config.bpsec.unwrap().bindings.len(), 1);
    }

    // No bpsec section is valid (default None).
    #[test]
    #[serial]
    fn no_bpsec_config() {
        let config = write_and_load("no-bpsec.yaml", "");
        assert!(config.bpsec.is_none());
    }

    // The removed `address` key of a tcpclv4 CLA entry is refused with the
    // replacement named: unknown keys are otherwise ignored, and a
    // deliberately loopback-only `address` must not silently escalate to
    // the default wildcard listener.
    #[test]
    #[serial]
    #[cfg(feature = "tcpclv4")]
    fn tcpclv4_address_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("address.yaml");
        std::fs::write(
            &path,
            "clas:\n  - name: tcp\n    type: tcpclv4\n    address: \"127.0.0.1:4556\"\n",
        )
        .unwrap();
        let Err(err) = Config::load(Some(path)) else {
            panic!("expected a parse error");
        };
        let err = err.to_string();
        assert!(err.contains("listeners"), "{err}");
    }

    // `null` is not a spelling in this schema: an earlier schema used it
    // to disable keepalives, so it is refused with the replacement named
    // rather than silently mapped to the default.
    #[test]
    #[serial]
    #[cfg(feature = "tcpclv4")]
    fn tcpclv4_keepalive_null_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("null-keepalive.yaml");
        std::fs::write(
            &path,
            "clas:\n  - name: tcp\n    type: tcpclv4\n    keepalive-interval: null\n",
        )
        .unwrap();
        let Err(err) = Config::load(Some(path)) else {
            panic!("expected a parse error");
        };
        let err = err.to_string();
        assert!(err.contains("keepalive"), "{err}");
    }

    // The dial-only spelling is an empty listener list, distinct from the
    // absent key (the default listener); the assembly folds each entry
    // into `Tcpclv4Builder::listen`, so an empty list binds nothing.
    #[test]
    #[serial]
    #[cfg(feature = "tcpclv4")]
    fn tcpclv4_empty_listeners_is_dial_only() {
        let config = write_and_load(
            "dial_only.yaml",
            "clas:\n  - name: tcp\n    type: tcpclv4\n    listeners: []\n",
        );
        let cla::ClaType::TcpClv4(tcpclv4) = &config.clas[0].cla_type else {
            panic!("expected a tcpclv4 CLA entry");
        };
        assert_eq!(tcpclv4.listeners, Some(vec![]));
    }

    // Durations are humantime strings; zero and garbage are refused.
    #[test]
    #[cfg(feature = "postgres-storage")]
    fn non_zero_duration_round_trips() {
        let duration: NonZeroDuration = serde_json::from_str("\"1m 30s\"").unwrap();
        assert_eq!(duration.get(), std::time::Duration::from_secs(90));
        assert_eq!(serde_json::to_string(&duration).unwrap(), "\"1m 30s\"");

        let err = serde_json::from_str::<NonZeroDuration>("\"0s\"")
            .unwrap_err()
            .to_string();
        assert!(err.contains("greater than zero"), "{err}");

        let err = serde_json::from_str::<NonZeroDuration>("\"ten minutes\"")
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid duration"), "{err}");
    }
}
