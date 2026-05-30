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

    #[serde(default)]
    pub outbox: Option<PathBuf>,

    #[serde(default)]
    pub inbox: Option<PathBuf>,
}

impl Config {
    pub fn load(config_file: Option<PathBuf>) -> anyhow::Result<Config> {
        let config_file = config_file
            .or_else(|| {
                std::env::var("HARDY_FILE_SERVICE_CONFIG_FILE")
                    .ok()
                    .map(PathBuf::from)
            })
            .unwrap_or_else(default_config_path);

        let source_file = config::File::with_name(&config_file.to_string_lossy());
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

        eprintln!("Loaded configuration from '{}'", config_file.display());
        Ok(config)
    }
}
