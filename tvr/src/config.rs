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

/// Returns the default config directory, platform-specific:
/// - Linux: /etc/hardy/
/// - macOS: /etc/hardy/
/// - Windows: %ProgramData%\hardy\ (via `directories` crate), or exe directory as fallback
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
    default_config_dir().join("tvr")
}

fn default_log_level() -> Level {
    Level::INFO
}

fn default_bpa_address() -> String {
    "http://[::1]:50051".to_string()
}

fn default_agent_name() -> String {
    "hardy-tvr".to_string()
}

fn default_priority() -> u32 {
    100
}

fn default_grpc_listen() -> std::net::SocketAddr {
    std::net::SocketAddr::new(std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST), 50052)
}

fn default_watch() -> bool {
    true
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
pub struct Config {
    /// Logging level (default: INFO)
    #[serde(default = "default_log_level", with = "log_level_serde")]
    pub log_level: Level,

    /// The address of the BPA gRPC server (e.g. "http://[::1]:50051")
    #[serde(default = "default_bpa_address")]
    pub bpa_address: String,

    /// Agent name registered with BPA (route source in FIB)
    #[serde(default = "default_agent_name")]
    pub agent_name: String,

    /// Default priority for contacts without explicit priority
    #[serde(default = "default_priority")]
    pub priority: u32,

    /// Path to contact plan file. If omitted, no file source.
    #[serde(default)]
    pub contact_plan: Option<PathBuf>,

    /// Monitor contact plan file for changes
    #[serde(default = "default_watch")]
    pub watch: bool,

    /// TVR gRPC service listen address
    #[serde(default = "default_grpc_listen")]
    pub grpc_listen: std::net::SocketAddr,
}

impl Config {
    pub fn load(config_file: Option<PathBuf>) -> anyhow::Result<Config> {
        let config_file = config_file
            .or_else(|| {
                std::env::var("HARDY_TVR_CONFIG_FILE")
                    .ok()
                    .map(PathBuf::from)
            })
            .unwrap_or_else(default_config_path);

        let source_file = config::File::with_name(&config_file.to_string_lossy());
        let source_env = config::Environment::with_prefix("HARDY_TVR")
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

    /// Helper: write a config file and load it.
    fn write_and_load(name: &str, content: &str) -> Config {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        Config::load(Some(path)).unwrap()
    }

    /// Empty config file produces sensible defaults.
    #[test]
    #[serial]
    fn empty_config_has_defaults() {
        let config = write_and_load("empty.yaml", "");
        assert_eq!(config.log_level, Level::INFO);
        assert_eq!(config.bpa_address, "http://[::1]:50051");
        assert_eq!(config.agent_name, "hardy-tvr");
        assert_eq!(config.priority, 100);
        assert!(config.contact_plan.is_none());
        assert!(config.watch);
        assert_eq!(
            config.grpc_listen,
            std::net::SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1], 50052))
        );
    }

    /// YAML config file overrides defaults.
    #[test]
    #[serial]
    fn yaml_overrides_defaults() {
        let config = write_and_load(
            "test.yaml",
            r#"
log-level: debug
bpa-address: "http://10.0.0.1:50051"
agent-name: "my-tvr"
priority: 200
contact-plan: "/etc/hardy/contacts"
watch: false
grpc-listen: "[::]:9999"
"#,
        );
        assert_eq!(config.log_level, Level::DEBUG);
        assert_eq!(config.bpa_address, "http://10.0.0.1:50051");
        assert_eq!(config.agent_name, "my-tvr");
        assert_eq!(config.priority, 200);
        assert_eq!(
            config.contact_plan.unwrap(),
            PathBuf::from("/etc/hardy/contacts")
        );
        assert!(!config.watch);
    }

    /// TOML config file works identically to YAML.
    #[test]
    #[serial]
    fn toml_config() {
        let config = write_and_load(
            "test.toml",
            r#"
log-level = "warn"
bpa-address = "http://10.0.0.2:50051"
agent-name = "toml-tvr"
priority = 50
"#,
        );
        assert_eq!(config.log_level, Level::WARN);
        assert_eq!(config.bpa_address, "http://10.0.0.2:50051");
        assert_eq!(config.agent_name, "toml-tvr");
        assert_eq!(config.priority, 50);
    }

    /// JSON config file works identically to YAML.
    #[test]
    #[serial]
    fn json_config() {
        let config = write_and_load(
            "test.json",
            r#"{
    "log-level": "error",
    "agent-name": "json-tvr",
    "priority": 1
}"#,
        );
        assert_eq!(config.log_level, Level::ERROR);
        assert_eq!(config.agent_name, "json-tvr");
        assert_eq!(config.priority, 1);
    }

    /// Environment variables override config file values.
    #[test]
    #[serial]
    fn env_overrides_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.yaml");
        std::fs::write(&path, "log-level: info\nagent-name: file-tvr\n").unwrap();

        unsafe { std::env::set_var("HARDY_TVR_LOG_LEVEL", "debug") };
        unsafe { std::env::set_var("HARDY_TVR_AGENT_NAME", "env-tvr") };
        let config = Config::load(Some(path)).unwrap();
        unsafe { std::env::remove_var("HARDY_TVR_LOG_LEVEL") };
        unsafe { std::env::remove_var("HARDY_TVR_AGENT_NAME") };

        assert_eq!(
            config.log_level,
            Level::DEBUG,
            "env var should override log level"
        );
        assert_eq!(
            config.agent_name, "env-tvr",
            "env var should override agent name"
        );
    }

    /// Missing config file returns an error.
    #[test]
    #[serial]
    fn missing_config_file_errors() {
        let result = Config::load(Some(PathBuf::from("/nonexistent/path/config")));
        assert!(result.is_err());
    }

    /// Invalid log level in config file returns an error.
    #[test]
    #[serial]
    fn invalid_log_level_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        std::fs::write(&path, "log-level: banana\n").unwrap();
        let result = Config::load(Some(path));
        assert!(result.is_err());
    }

    /// Malformed YAML returns an error.
    #[test]
    #[serial]
    fn malformed_yaml_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        std::fs::write(&path, "agent-name: [broken\n").unwrap();
        let result = Config::load(Some(path));
        assert!(result.is_err());
    }

    /// Malformed TOML returns an error.
    #[test]
    #[serial]
    fn malformed_toml_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "log-level = \n").unwrap();
        let result = Config::load(Some(path));
        assert!(result.is_err());
    }

    /// Unknown fields are silently ignored.
    #[test]
    #[serial]
    fn unknown_fields_ignored() {
        let config = write_and_load(
            "extra.yaml",
            r#"
log-level: warn
this-field-does-not-exist: 42
"#,
        );
        assert_eq!(config.log_level, Level::WARN);
    }

    /// Contact plan path is preserved.
    #[test]
    #[serial]
    fn contact_plan_path() {
        let config = write_and_load("plan.yaml", "contact-plan: /tmp/contacts.txt\n");
        assert_eq!(
            config.contact_plan.unwrap(),
            PathBuf::from("/tmp/contacts.txt")
        );
    }

    /// Watch can be disabled.
    #[test]
    #[serial]
    fn watch_disabled() {
        let config = write_and_load("nowatch.yaml", "watch: false\n");
        assert!(!config.watch);
    }

    /// Priority zero is valid.
    #[test]
    #[serial]
    fn priority_zero() {
        let config = write_and_load("prio.yaml", "priority: 0\n");
        assert_eq!(config.priority, 0);
    }
}
