use serde::{Deserialize, Deserializer};
use std::path::PathBuf;
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

    /// Agent name registered with BPA (route source in FIB)
    pub agent_name: String,

    /// Default priority for contacts without explicit priority
    pub priority: u32,

    /// Path to contact plan file. If omitted, no file source.
    pub contact_plan: Option<PathBuf>,

    /// Monitor contact plan file for changes
    pub watch: bool,

    /// TVR gRPC service listen address
    pub grpc_listen: std::net::SocketAddr,

    /// Logging level (e.g. "info", "debug", "trace")
    #[serde(deserialize_with = "log_level_serde::deserialize")]
    pub log_level: Option<Level>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bpa_address: "http://[::1]:50051".to_string(),
            agent_name: "hardy-tvr".to_string(),
            priority: 100,
            contact_plan: None,
            watch: true,
            grpc_listen: std::net::SocketAddr::new(
                std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
                50052,
            ),
            log_level: None,
        }
    }
}

impl Config {
    pub fn load(config_file: Option<String>) -> anyhow::Result<Config> {
        let config_file = config_file
            .or_else(|| std::env::var("HARDY_TVR_CONFIG_FILE").ok())
            .unwrap_or_else(|| "hardy-tvr".to_string());

        let source_file = config::File::with_name(&config_file).required(false);
        let source_env = config::Environment::with_prefix("HARDY_TVR")
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
