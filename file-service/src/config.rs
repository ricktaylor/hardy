use core::time::Duration;
use std::path::PathBuf;

use hardy_bpv7::eid::Eid;
use serde::{Deserialize, Serialize};
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

fn default_config_path() -> PathBuf {
    PathBuf::from("/etc/hardy/file-service")
}

fn default_log_level() -> Level {
    Level::INFO
}

fn default_bpa_address() -> String {
    "http://[::1]:50051".to_string()
}

fn default_outbox() -> PathBuf {
    PathBuf::from("/tmp/hardy/outbox")
}

fn default_errors() -> PathBuf {
    PathBuf::from("/tmp/hardy/errors")
}

fn default_inbox() -> PathBuf {
    PathBuf::from("/tmp/hardy/inbox")
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
pub struct Config {
    #[serde(default = "default_log_level", with = "log_level_serde")]
    pub log_level: Level,

    #[serde(default = "default_bpa_address")]
    pub bpa_address: String,

    #[serde(default)]
    pub service_id: Option<u32>,

    #[serde(default)]
    pub destination: Option<Eid>,

    #[serde(default, with = "humantime_serde")]
    pub lifetime: Option<Duration>,

    #[serde(default = "default_outbox")]
    pub outbox: PathBuf,

    #[serde(default = "default_errors")]
    pub errors: PathBuf,

    #[serde(default = "default_inbox")]
    pub inbox: PathBuf,
}

impl Config {
    pub fn load(config_file: Option<PathBuf>) -> anyhow::Result<Config> {
        let (config_file, required) = match config_file.or_else(|| {
            std::env::var("HARDY_FILE_SERVICE_CONFIG_FILE")
                .ok()
                .map(PathBuf::from)
        }) {
            Some(path) => (path, true),
            None => (default_config_path(), false),
        };

        let source_file =
            config::File::with_name(&config_file.to_string_lossy()).required(required);
        let source_env = config::Environment::with_prefix("HARDY_FILE_SERVICE")
            .prefix_separator("_")
            .separator("__")
            .convert_case(config::Case::Kebab)
            .try_parsing(true);

        let config = config::Config::builder()
            .add_source(source_file)
            .add_source(source_env)
            .build()?
            .try_deserialize()?;

        if required {
            eprintln!("Loaded configuration from '{}'", config_file.display());
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn write_and_load(name: &str, content: &str) -> Config {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        Config::load(Some(path)).unwrap()
    }

    #[test]
    #[serial]
    fn empty_config_has_defaults() {
        let config = write_and_load("empty.yaml", "");
        assert_eq!(config.log_level, Level::INFO);
        assert_eq!(config.bpa_address, "http://[::1]:50051");
        assert!(config.service_id.is_none());
        assert!(config.destination.is_none());
        assert!(config.lifetime.is_none());
        assert_eq!(config.outbox, PathBuf::from("/tmp/hardy/outbox"));
        assert_eq!(config.errors, PathBuf::from("/tmp/hardy/errors"));
        assert_eq!(config.inbox, PathBuf::from("/tmp/hardy/inbox"));
    }

    #[test]
    #[serial]
    fn yaml_overrides_defaults() {
        let config = write_and_load(
            "test.yaml",
            r#"
bpa-address: "http://10.0.0.1:50051"
log-level: "debug"
service-id: 99
destination: "ipn:5.42"
lifetime: "1h"
outbox: /tmp/out
inbox: /tmp/in
"#,
        );
        assert_eq!(config.log_level, Level::DEBUG);
        assert_eq!(config.bpa_address, "http://10.0.0.1:50051");
        assert_eq!(config.service_id, Some(99));
        assert!(config.destination.is_some());
        assert_eq!(config.lifetime, Some(Duration::from_secs(3600)));
        assert_eq!(config.outbox, PathBuf::from("/tmp/out"));
        assert_eq!(config.inbox, PathBuf::from("/tmp/in"));
    }

    #[test]
    #[serial]
    fn env_overrides_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.yaml");
        std::fs::write(
            &path,
            "log-level: info\nbpa-address: \"http://file:50051\"\n",
        )
        .unwrap();

        unsafe { std::env::set_var("HARDY_FILE_SERVICE_LOG_LEVEL", "error") };
        unsafe { std::env::set_var("HARDY_FILE_SERVICE_BPA_ADDRESS", "http://env:50051") };
        let config = Config::load(Some(path)).unwrap();
        unsafe { std::env::remove_var("HARDY_FILE_SERVICE_LOG_LEVEL") };
        unsafe { std::env::remove_var("HARDY_FILE_SERVICE_BPA_ADDRESS") };

        assert_eq!(config.log_level, Level::ERROR);
        assert_eq!(config.bpa_address, "http://env:50051");
    }

    #[test]
    #[serial]
    fn missing_explicit_config_errors() {
        let result = Config::load(Some(PathBuf::from("/nonexistent/path/config")));
        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn missing_default_config_uses_defaults() {
        let result = Config::load(None);
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn humantime_lifetime_parsing() {
        let config = write_and_load("lifetime.yaml", "lifetime: \"30m\"\n");
        assert_eq!(config.lifetime, Some(Duration::from_secs(1800)));
    }

    #[test]
    #[serial]
    fn invalid_log_level_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        std::fs::write(&path, "log-level: banana\n").unwrap();
        let result = Config::load(Some(path));
        assert!(result.is_err());
    }
}
