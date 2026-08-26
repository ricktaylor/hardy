use core::num::NonZeroUsize;
#[cfg(feature = "grpc")]
use std::net::SocketAddr;
use std::time::Duration;
use std::{collections::HashMap, path::PathBuf};

use hardy_async::watcher::WatchMode;
use hardy_bpa::node_ids::NodeIds;
use hardy_bpv7::eid::Service;
use serde::{Deserialize, Serialize};
use tracing::Level;

#[cfg(feature = "grpc")]
use crate::config::tls::GrpcTlsConfig;
use crate::error::Error;

pub mod bpsec;
pub mod cla;
pub mod storage;
pub mod tls;

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

fn default_drain_timeout() -> Duration {
    Duration::from_secs(5)
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

// A duration written as a humantime string (e.g. `5s`, `1m 30s`);
// zero is allowed, meaning the wait is disabled outright.
mod human_duration {
    use std::time::Duration;

    use serde::Deserialize;

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        humantime::parse_duration(&text)
            .map_err(|e| serde::de::Error::custom(format_args!("invalid duration: {e}")))
    }

    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.collect_str(&humantime::format_duration(*duration))
    }
}

// A positive duration, written as a humantime string (e.g. `30s`, `10m`,
// `1h 30m`).
#[cfg(feature = "postgres-storage")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NonZeroDuration(Duration);

#[cfg(feature = "postgres-storage")]
impl NonZeroDuration {
    pub fn new(duration: Duration) -> Option<Self> {
        (!duration.is_zero()).then_some(Self(duration))
    }

    pub fn get(&self) -> Duration {
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

// A BPA registration surface the gRPC front end can host. Listed by
// name in `grpc.services`; an unknown name is refused at parse with the
// known ones listed.
#[cfg(feature = "grpc")]
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum GrpcService {
    // Remote convergence-layer adapters.
    Cla,
    // Remote low-level services, which exchange whole BPv7 bundles.
    Service,
    // Remote applications, which exchange payloads (ADUs).
    Application,
    // Remote routing agents.
    Routing,
}

// The service list must name at least one surface and each at most once:
// an empty list is a misconfiguration (omit the `grpc` section to run no
// gRPC server), and a repeated surface is a mistake, not a doubled mount.
#[cfg(feature = "grpc")]
fn at_least_one_service<'de, D>(deserializer: D) -> Result<Vec<GrpcService>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let names = Vec::<String>::deserialize(deserializer)?;
    if names.is_empty() {
        return Err(serde::de::Error::custom(
            "grpc.services must list at least one service",
        ));
    }
    let mut services = Vec::with_capacity(names.len());
    for name in names {
        let service = match name.as_str() {
            "cla" => GrpcService::Cla,
            "service" => GrpcService::Service,
            "application" => GrpcService::Application,
            "routing" => GrpcService::Routing,
            other => {
                return Err(serde::de::Error::custom(format!(
                    "grpc.services has unknown service {other:?}, expected one of cla, service, application, routing"
                )));
            }
        };
        if services.contains(&service) {
            return Err(serde::de::Error::custom(format!(
                "grpc.services lists {service:?} more than once"
            )));
        }
        services.push(service);
    }
    Ok(services)
}

/// An HTTP/2 flow-control window size in bytes.
#[cfg(feature = "grpc")]
#[derive(Serialize, Debug, Clone, Copy)]
pub struct Http2WindowSize(u32);

#[cfg(feature = "grpc")]
impl Http2WindowSize {
    /// The RFC 9113 Section 6.9.1 maximum: `2^31 - 1` bytes.
    pub const MAX: Http2WindowSize = Http2WindowSize(i32::MAX as u32);

    /// Creates a window size; `None` above [`MAX`](Self::MAX).
    pub const fn new(bytes: u32) -> Option<Self> {
        if bytes > Self::MAX.0 {
            None
        } else {
            Some(Self(bytes))
        }
    }

    /// The window size in bytes.
    pub const fn get(self) -> u32 {
        self.0
    }
}

#[cfg(feature = "grpc")]
impl<'de> Deserialize<'de> for Http2WindowSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let bytes = u32::deserialize(deserializer)?;
        Self::new(bytes).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "HTTP/2 window size {bytes} exceeds the maximum of {}",
                Self::MAX.get()
            ))
        })
    }
}

/// An HTTP/2 maximum frame size in bytes.
#[cfg(feature = "grpc")]
#[derive(Serialize, Debug, Clone, Copy)]
pub struct Http2FrameSize(u32);

#[cfg(feature = "grpc")]
impl Http2FrameSize {
    /// The RFC 9113 Section 6.5.2 minimum: `2^14` bytes.
    pub const MIN: Http2FrameSize = Http2FrameSize(1 << 14);
    /// The RFC 9113 Section 6.5.2 maximum: `2^24 - 1` bytes.
    pub const MAX: Http2FrameSize = Http2FrameSize((1 << 24) - 1);

    /// Creates a frame size; `None` outside
    /// [`MIN`](Self::MIN)`..=`[`MAX`](Self::MAX).
    pub const fn new(bytes: u32) -> Option<Self> {
        if bytes < Self::MIN.0 || bytes > Self::MAX.0 {
            None
        } else {
            Some(Self(bytes))
        }
    }

    /// The frame size in bytes.
    pub const fn get(self) -> u32 {
        self.0
    }
}

#[cfg(feature = "grpc")]
impl<'de> Deserialize<'de> for Http2FrameSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let bytes = u32::deserialize(deserializer)?;
        Self::new(bytes).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "HTTP/2 max-frame-size {bytes} must be between {} and {}",
                Self::MIN.get(),
                Self::MAX.get()
            ))
        })
    }
}

// HTTP/2 transport tuning for the gRPC listener: what the operator wrote,
// with every key optional. Absent keys defer to the server's own defaults
// (applied in `grpc.rs`), which favour throughput at scale (a single large
// transfer is otherwise capped at `window / round-trip-time` by the fixed
// ~64 KiB default window). All sizes are in bytes.
#[cfg(feature = "grpc")]
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct GrpcHttp2Config {
    // Auto-size the stream and connection flow-control windows to the
    // connection's bandwidth-delay product; absent defers to the server
    // default (on). When on, the fixed `initial-*-window-size` keys below
    // are ignored; set this `false` to pin fixed windows instead.
    pub adaptive_window: Option<bool>,

    // Fixed initial per-stream receive window; absent uses the transport
    // default. Ignored while `adaptive-window` is on.
    pub initial_stream_window_size: Option<Http2WindowSize>,

    // Fixed initial whole-connection receive window; absent uses the
    // transport default. Ignored while `adaptive-window` is on.
    pub initial_connection_window_size: Option<Http2WindowSize>,

    // Maximum concurrent HTTP/2 streams a peer may open; absent uses the
    // transport default. Bounds per-connection memory (window x streams).
    pub max_concurrent_streams: Option<u32>,

    // Maximum HTTP/2 DATA frame payload; absent defers to the server
    // default (one chunk per frame). Larger frames cut per-frame
    // bookkeeping for big transfers.
    pub max_frame_size: Option<Http2FrameSize>,
}

// The `grpc` section: the gRPC front end serving BPA registration to
// remote CLAs, services, applications, and routing agents. Absent runs
// no gRPC server. The transport is owned and assembled by this crate
// (`hardy-proto` provides the per-surface services), so the defaults are
// this crate's own.
#[cfg(feature = "grpc")]
#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct GrpcConfig {
    // The listen address; absent defers to the server default
    // (`[::1]:50051`).
    #[serde(default)]
    pub address: Option<SocketAddr>,

    // The services to expose, e.g. `["application", "cla", "service",
    // "routing"]`; required and non-empty.
    #[serde(deserialize_with = "at_least_one_service")]
    pub services: Vec<GrpcService>,

    // How long a graceful shutdown waits for open gRPC connections to
    // drain before abandoning them, as a humantime string; `0s` cuts
    // them immediately. The drain is shutdown's one unbounded wait: a
    // client holding an unread response stream keeps its connection
    // open indefinitely.
    #[serde(default = "default_drain_timeout", with = "human_duration")]
    pub drain_timeout: Duration,

    // TLS for the listener; absent serves plaintext HTTP/2.
    #[serde(default)]
    pub tls: Option<GrpcTlsConfig>,

    // HTTP/2 transport tuning; absent defers to the server defaults.
    #[serde(default)]
    pub http2: GrpcHttp2Config,
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

    // Maximum size in bytes of a single reassembled bundle at ingress or
    // origination; absent defers to the BPA default.
    #[serde(default)]
    pub max_bundle_size: Option<NonZeroUsize>,

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
    pub grpc: Option<GrpcConfig>,

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
        assert_eq!(tls.client_auth, super::tls::ClientAuth::Required);
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

    // A `grpc` section enabling no services is refused at parse: absent
    // and all-off are different spellings, and only an absent section
    // means "no gRPC server".
    #[test]
    #[serial]
    #[cfg(feature = "grpc")]
    fn grpc_no_services_is_refused() {
        let dir = tempfile::tempdir().unwrap();

        let path = dir.path().join("all-off.yaml");
        std::fs::write(&path, "grpc:\n  services: []\n").unwrap();
        let Err(err) = Config::load(Some(path)) else {
            panic!("expected a parse error");
        };
        let err = err.to_string();
        assert!(err.contains("at least one"), "{err}");

        let path = dir.path().join("missing.yaml");
        std::fs::write(&path, "grpc:\n  address: \"[::1]:50051\"\n").unwrap();
        let Err(err) = Config::load(Some(path)) else {
            panic!("expected a parse error");
        };
        let err = err.to_string();
        assert!(err.contains("services"), "{err}");
    }

    // Unknown gRPC service names are refused at parse with the known ones
    // listed (previously ignored with a warning at startup).
    #[test]
    #[serial]
    #[cfg(feature = "grpc")]
    fn grpc_unknown_service_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grpc.yaml");
        std::fs::write(&path, "grpc:\n  services: [\"clas\"]\n").unwrap();
        let Err(err) = Config::load(Some(path)) else {
            panic!("expected a parse error");
        };
        let err = err.to_string();
        assert!(err.contains("application"), "{err}");
    }

    // The service list parses into the typed surfaces it names.
    #[test]
    #[serial]
    #[cfg(feature = "grpc")]
    fn grpc_services_list() {
        use super::GrpcService;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("list.yaml");
        std::fs::write(&path, "grpc:\n  services: [\"application\", \"routing\"]\n").unwrap();

        let config = Config::load(Some(path)).expect("the service list must parse");
        let services = config.grpc.expect("grpc section must be present").services;
        assert_eq!(
            services,
            vec![GrpcService::Application, GrpcService::Routing]
        );
    }

    // The drain timeout is a humantime string: defaulted when absent,
    // zero allowed (cut connections immediately), garbage refused.
    #[test]
    #[cfg(feature = "grpc")]
    fn grpc_drain_timeout_parses_as_humantime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");

        std::fs::write(&path, "grpc:\n  services: [\"application\"]\n").unwrap();
        let config = Config::load(Some(path.clone())).unwrap();
        assert_eq!(config.grpc.unwrap().drain_timeout, Duration::from_secs(5));

        std::fs::write(
            &path,
            "grpc:\n  services: [\"application\"]\n  drain-timeout: 0s\n",
        )
        .unwrap();
        let config = Config::load(Some(path.clone())).unwrap();
        assert_eq!(config.grpc.unwrap().drain_timeout, Duration::ZERO);

        std::fs::write(
            &path,
            "grpc:\n  services: [\"application\"]\n  drain-timeout: eventually\n",
        )
        .unwrap();
        assert!(Config::load(Some(path)).is_err());
    }

    // A repeated surface is a mistake, not a doubled mount.
    #[test]
    #[serial]
    #[cfg(feature = "grpc")]
    fn grpc_duplicate_service_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dup.yaml");
        std::fs::write(&path, "grpc:\n  services: [\"cla\", \"cla\"]\n").unwrap();
        let Err(err) = Config::load(Some(path)) else {
            panic!("expected a parse error");
        };
        let err = err.to_string();
        assert!(err.contains("more than once"), "{err}");
    }

    // HTTP/2 tuning values outside their protocol ranges are refused at
    // parse by their newtypes.
    #[test]
    #[serial]
    #[cfg(feature = "grpc")]
    fn grpc_http2_out_of_range_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("http2.yaml");

        // In-range values parse and round-trip through the newtypes.
        std::fs::write(
            &path,
            "grpc:\n  services: [\"application\"]\n  http2:\n    max-frame-size: 1048576\n    initial-stream-window-size: 16777216\n",
        )
        .unwrap();
        let http2 = Config::load(Some(path.clone()))
            .unwrap()
            .grpc
            .unwrap()
            .http2;
        assert_eq!(http2.max_frame_size.unwrap().get(), 1048576);
        assert_eq!(http2.initial_stream_window_size.unwrap().get(), 16777216);

        // A frame size below HTTP/2's 2^14 minimum is refused.
        std::fs::write(
            &path,
            "grpc:\n  services: [\"application\"]\n  http2:\n    max-frame-size: 1024\n",
        )
        .unwrap();
        assert!(Config::load(Some(path.clone())).is_err());

        // A window size above HTTP/2's 2^31 - 1 maximum is refused.
        std::fs::write(
            &path,
            "grpc:\n  services: [\"application\"]\n  http2:\n    initial-stream-window-size: 4294967295\n",
        )
        .unwrap();
        assert!(Config::load(Some(path)).is_err());
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
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        Config::load(Some(root.join("config.yaml"))).expect("the shipped config.yaml must parse");
        Config::load(Some(root.join("examples/config.yaml")))
            .expect("the example config.yaml must parse");
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
        assert_eq!(duration.get(), Duration::from_secs(90));
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
