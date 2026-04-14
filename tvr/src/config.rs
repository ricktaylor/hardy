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
            .map(|p| p.to_string_lossy().into_owned())
            .or_else(|| std::env::var("HARDY_TVR_CONFIG_FILE").ok())
            .unwrap_or_else(|| default_config_path().to_string_lossy().into_owned());

        let source_file = config::File::with_name(&config_file);
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

        eprintln!("Loaded configuration from '{config_file}'");
        Ok(config)
    }
}
