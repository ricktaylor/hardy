use serde::{Deserialize, Deserializer};
use std::str::FromStr;
use tracing::Level;

mod log_level_serde {
    use super::*;

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Level>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: Option<String> = Option::deserialize(deserializer)?;
        s.map(|s| Level::from_str(&s).map_err(serde::de::Error::custom))
            .transpose()
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Config {
    /// The address of the BPA gRPC server (e.g. "http://[::1]:50051")
    pub bpa_address: String,

    /// The name of this CLA instance to register with the BPA
    pub cla_name: String,

    /// Logging level (e.g. "info", "debug", "trace")
    #[serde(default, deserialize_with = "log_level_serde::deserialize")]
    pub log_level: Option<Level>,

    /// TCPCLv4 configuration
    #[serde(flatten)]
    pub tcpcl: hardy_tcpclv4::config::Config,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bpa_address: "http://[::1]:50051".to_string(),
            cla_name: env!("CARGO_PKG_NAME").to_string(),
            log_level: None,
            tcpcl: Default::default(),
        }
    }
}

impl Config {
    pub fn load(config_file: Option<String>) -> anyhow::Result<Config> {
        let config_file = config_file
            .or_else(|| std::env::var("HARDY_TCPCLV4_CONFIG_FILE").ok())
            .unwrap_or_else(|| "hardy-tcpclv4".to_string());

        let source_file = config::File::with_name(&config_file).required(false);
        let source_env = config::Environment::with_prefix("HARDY_TCPCLV4")
            .prefix_separator("_")
            .separator("__")
            .convert_case(config::Case::Kebab);

        let config = config::Config::builder()
            .add_source(source_file)
            .add_source(source_env)
            .build()?
            .try_deserialize()?;

        eprintln!("Loaded configuration from '{config_file}'");
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// CFG-01: No config file → valid defaults (aligned with TVR/bpa-server pattern).
    #[test]
    fn test_default_config() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("empty.toml");
        std::fs::write(&config_path, "").unwrap();

        let config = Config::load(Some(config_path.to_string_lossy().into_owned())).unwrap();
        assert_eq!(config.bpa_address, "http://[::1]:50051");
        assert_eq!(config.cla_name, env!("CARGO_PKG_NAME"));
        assert!(config.log_level.is_none());
        assert_eq!(
            config.tcpcl.address.unwrap(),
            std::net::SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 0], 4556))
        );
    }

    /// CFG-02: TOML config file provides all required fields and overrides defaults.
    #[test]
    fn test_toml_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("test.toml");
        let mut f = std::fs::File::create(&config_path).unwrap();
        writeln!(
            f,
            r#"
bpa-address = "http://10.0.0.1:50051"
cla-name = "test-cla"
log-level = "debug"
address = "0.0.0.0:9999"
segment-mru = 8192
keepalive-interval = 30
"#
        )
        .unwrap();

        let config = Config::load(Some(config_path.to_string_lossy().into_owned())).unwrap();
        assert_eq!(config.bpa_address, "http://10.0.0.1:50051");
        assert_eq!(config.cla_name, "test-cla");
        assert_eq!(config.log_level.unwrap(), Level::DEBUG);
        assert_eq!(
            config.tcpcl.address.unwrap(),
            std::net::SocketAddr::from(([0, 0, 0, 0], 9999))
        );
        assert_eq!(config.tcpcl.segment_mru, 8192);
        assert_eq!(config.tcpcl.session_defaults.keepalive_interval, Some(30));
    }

    /// CFG-03: Environment variables override config file values.
    #[test]
    fn test_env_override() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("test.toml");
        let mut f = std::fs::File::create(&config_path).unwrap();
        writeln!(
            f,
            r#"
bpa-address = "http://file-value:50051"
cla-name = "file-cla"
"#
        )
        .unwrap();

        // SAFETY: test runs single-threaded; no other thread reads this env var
        unsafe { std::env::set_var("HARDY_TCPCLV4_BPA_ADDRESS", "http://env-value:50051") };
        let config = Config::load(Some(config_path.to_string_lossy().into_owned())).unwrap();
        unsafe { std::env::remove_var("HARDY_TCPCLV4_BPA_ADDRESS") };

        assert_eq!(
            config.bpa_address, "http://env-value:50051",
            "env var should override file value"
        );
        assert_eq!(
            config.cla_name, "file-cla",
            "non-overridden value should come from file"
        );
    }
}
