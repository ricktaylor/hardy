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
#[serde(rename_all = "kebab-case")]
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

pub fn load(path: Option<PathBuf>) -> anyhow::Result<Config> {
    let mut builder = config::Config::builder();

    if let Some(path) = path {
        builder = builder.add_source(config::File::from(path));
    } else {
        builder = builder.add_source(
            config::File::from(std::path::Path::new("hardy-tcpclv4.yaml")).required(false),
        );
    }

    builder = builder.add_source(config::Environment::with_prefix("HARDY_TCPCLV4"));

    builder.build()?.try_deserialize().map_err(Into::into)
}
