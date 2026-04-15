use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::Level;

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

// Returns the default config directory, platform-specific:
// - Linux: /etc/hardy/
// - macOS: /etc/hardy/
// - Windows: %ProgramData%\hardy\ (via `directories` crate), or exe directory as fallback
fn default_config_dir() -> PathBuf {
    #[cfg(unix)]
    return PathBuf::from("/etc/hardy");

    #[cfg(windows)]
    return directories::BaseDirs::new()
        .map(|dirs| dirs.data_local_dir().join("hardy"))
        .unwrap_or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|p| p.to_path_buf()))
                .unwrap_or_else(|| PathBuf::from("."))
        });
}

fn default_config_path() -> PathBuf {
    default_config_dir().join("tcpclv4")
}

fn default_log_level() -> Level {
    Level::INFO
}

fn default_bpa_address() -> String {
    "http://[::1]:50051".to_string()
}

fn default_cla_name() -> String {
    env!("CARGO_PKG_NAME").to_string()
}

// Configuration for the standalone TCPCLv4 CLA server.
//
// Loaded from a TOML/YAML/JSON config file and/or environment variables
// prefixed with `HARDY_TCPCLV4_`. Uses kebab-case field names in config files
// and `__` as the nested-field separator for environment variables.
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
pub struct Config {
    // Logging level for the tracing subscriber.
    //
    // Default: `INFO`.
    #[serde(default = "default_log_level", with = "log_level_serde")]
    pub log_level: Level,

    // The gRPC endpoint of the BPA to register with.
    //
    // Default: `"http://[::1]:50051"`.
    #[serde(default = "default_bpa_address")]
    pub bpa_address: String,

    // The name used to identify this CLA instance when registering with the BPA.
    //
    // Default: the crate package name (`"tcpclv4-server"`).
    #[serde(default = "default_cla_name")]
    pub cla_name: String,

    // TCPCLv4 transport-layer configuration (flattened into the top level).
    #[serde(flatten)]
    pub tcpcl: hardy_tcpclv4::config::Config,
}

impl Config {
    // Load configuration from a file and environment variable overrides.
    //
    // Resolution order for the config file path:
    // 1. The explicit `config_file` argument (if `Some`).
    // 2. The `HARDY_TCPCLV4_CONFIG_FILE` environment variable (if set).
    // 3. The platform-specific default path (e.g. `/etc/hardy/tcpclv4` on Linux).
    //
    // Environment variables prefixed with `HARDY_TCPCLV4_` override values
    // from the config file.
    pub fn load(config_file: Option<PathBuf>) -> anyhow::Result<Config> {
        let config_file = config_file
            .or_else(|| {
                std::env::var("HARDY_TCPCLV4_CONFIG_FILE")
                    .ok()
                    .map(PathBuf::from)
            })
            .unwrap_or_else(default_config_path);

        let source_file = config::File::with_name(&config_file.to_string_lossy());
        let source_env = config::Environment::with_prefix("HARDY_TCPCLV4")
            .prefix_separator("_")
            .separator("__")
            .convert_case(config::Case::Kebab)
            .try_parsing(true);

        let config = config::Config::builder()
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
    use std::io::Write;

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
        let config = write_and_load("empty.toml", "");
        assert_eq!(config.bpa_address, "http://[::1]:50051");
        assert_eq!(config.cla_name, env!("CARGO_PKG_NAME"));
        assert_eq!(config.log_level, Level::INFO);
        assert_eq!(
            config.tcpcl.address.unwrap(),
            std::net::SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 0], 4556))
        );
        assert_eq!(config.tcpcl.segment_mru, 16384);
        assert!(!config.tcpcl.session_defaults.require_tls);
    }

    // TOML config file overrides defaults.
    #[test]
    #[serial]
    fn toml_overrides_defaults() {
        let config = write_and_load(
            "test.toml",
            r#"
bpa-address = "http://10.0.0.1:50051"
cla-name = "test-cla"
log-level = "debug"
address = "0.0.0.0:9999"
segment-mru = 8192
keepalive-interval = 30
"#,
        );
        assert_eq!(config.bpa_address, "http://10.0.0.1:50051");
        assert_eq!(config.cla_name, "test-cla");
        assert_eq!(config.log_level, Level::DEBUG);
        assert_eq!(
            config.tcpcl.address.unwrap(),
            std::net::SocketAddr::from(([0, 0, 0, 0], 9999))
        );
        assert_eq!(config.tcpcl.segment_mru, 8192);
        assert_eq!(config.tcpcl.session_defaults.keepalive_interval, Some(30));
    }

    // YAML config file works identically to TOML.
    #[test]
    #[serial]
    fn yaml_config() {
        let config = write_and_load(
            "test.yaml",
            r#"
bpa-address: "http://10.0.0.2:50051"
cla-name: "yaml-cla"
log-level: "warn"
segment-mru: 4096
"#,
        );
        assert_eq!(config.bpa_address, "http://10.0.0.2:50051");
        assert_eq!(config.cla_name, "yaml-cla");
        assert_eq!(config.log_level, Level::WARN);
        assert_eq!(config.tcpcl.segment_mru, 4096);
    }

    // JSON config file works identically to TOML.
    #[test]
    #[serial]
    fn json_config() {
        let config = write_and_load(
            "test.json",
            r#"{
    "bpa-address": "http://10.0.0.3:50051",
    "cla-name": "json-cla",
    "log-level": "error"
}"#,
        );
        assert_eq!(config.bpa_address, "http://10.0.0.3:50051");
        assert_eq!(config.cla_name, "json-cla");
        assert_eq!(config.log_level, Level::ERROR);
    }

    // Environment variables override config file values.
    #[test]
    #[serial]
    fn env_overrides_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"
bpa-address = "http://file-value:50051"
cla-name = "file-cla"
log-level = "warn"
"#
        )
        .unwrap();

        unsafe { std::env::set_var("HARDY_TCPCLV4_BPA_ADDRESS", "http://env-value:50051") };
        unsafe { std::env::set_var("HARDY_TCPCLV4_LOG_LEVEL", "error") };
        let config = Config::load(Some(path)).unwrap();
        unsafe { std::env::remove_var("HARDY_TCPCLV4_BPA_ADDRESS") };
        unsafe { std::env::remove_var("HARDY_TCPCLV4_LOG_LEVEL") };

        assert_eq!(
            config.bpa_address, "http://env-value:50051",
            "env var should override file"
        );
        assert_eq!(
            config.cla_name, "file-cla",
            "non-overridden value should come from file"
        );
        assert_eq!(
            config.log_level,
            Level::ERROR,
            "env var should override log level"
        );
    }

    // Nested env vars with __ separator override flattened tcpclv4 fields.
    #[test]
    #[serial]
    fn env_overrides_nested_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.toml");
        std::fs::write(&path, "").unwrap();

        unsafe { std::env::set_var("HARDY_TCPCLV4_SEGMENT_MRU", "32768") };
        let config = Config::load(Some(path)).unwrap();
        unsafe { std::env::remove_var("HARDY_TCPCLV4_SEGMENT_MRU") };

        assert_eq!(config.tcpcl.segment_mru, 32768);
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
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "log-level = \"banana\"").unwrap();
        let result = Config::load(Some(path));
        assert!(result.is_err());
    }

    // Negative segment-mru is rejected.
    #[test]
    #[serial]
    fn negative_segment_mru_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "segment-mru = -1").unwrap();
        let result = Config::load(Some(path));
        assert!(result.is_err());
    }

    // Invalid address format is rejected.
    #[test]
    #[serial]
    fn invalid_address_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "address = \"not-an-address\"").unwrap();
        let result = Config::load(Some(path));
        assert!(result.is_err());
    }

    // TLS config with partial fields parses (cert without key is valid at config level).
    #[test]
    #[serial]
    fn tls_partial_config() {
        let config = write_and_load(
            "tls.yaml",
            r#"
require-tls: true
tls:
  cert-file: "/etc/hardy/certs/server.crt"
  private-key-file: "/etc/hardy/private/server.key"
"#,
        );
        assert!(config.tcpcl.session_defaults.require_tls);
        let tls = config.tcpcl.tls.unwrap();
        assert_eq!(
            tls.cert_file.unwrap(),
            PathBuf::from("/etc/hardy/certs/server.crt")
        );
        assert_eq!(
            tls.private_key_file.unwrap(),
            PathBuf::from("/etc/hardy/private/server.key")
        );
        assert!(tls.ca_certs.is_none());
    }

    // Malformed TOML returns an error.
    #[test]
    #[serial]
    fn malformed_toml_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "bpa-address = \n").unwrap();
        let result = Config::load(Some(path));
        assert!(result.is_err());
    }

    // Malformed YAML returns an error.
    #[test]
    #[serial]
    fn malformed_yaml_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        std::fs::write(&path, "bpa-address: [broken\n").unwrap();
        let result = Config::load(Some(path));
        assert!(result.is_err());
    }

    // Unknown fields are silently ignored.
    #[test]
    #[serial]
    fn unknown_fields_ignored() {
        let config = write_and_load(
            "extra.toml",
            r#"
log-level = "warn"
this-does-not-exist = 42
"#,
        );
        assert_eq!(config.log_level, Level::WARN);
    }

    // Large segment-mru value is accepted.
    #[test]
    #[serial]
    fn large_segment_mru() {
        let config = write_and_load("large.toml", "segment-mru = 1073741824\n");
        assert_eq!(config.tcpcl.segment_mru, 1073741824);
    }

    // Keepalive interval of 0 disables keepalives.
    #[test]
    #[serial]
    fn keepalive_zero() {
        let config = write_and_load("keepalive.toml", "keepalive-interval = 0\n");
        assert_eq!(config.tcpcl.session_defaults.keepalive_interval, Some(0));
    }
}
