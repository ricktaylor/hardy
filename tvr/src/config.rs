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

pub fn load(path: Option<PathBuf>) -> anyhow::Result<Config> {
    let mut builder = config::Config::builder();

    if let Some(path) = path {
        builder = builder.add_source(config::File::from(path));
    } else {
        builder = builder
            .add_source(config::File::from(std::path::Path::new("hardy-tvr.toml")).required(false));
    }

    builder = builder.add_source(config::Environment::with_prefix("HARDY_TVR"));

    builder.build()?.try_deserialize().map_err(Into::into)
}
