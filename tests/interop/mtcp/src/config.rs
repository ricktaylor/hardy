use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
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

fn default_log_level() -> Level {
    Level::INFO
}

fn default_max_bundle_size() -> u64 {
    0x4000_0000 // 1GB
}

/// Framing mode for the CLA.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Framing {
    /// MTCP: CBOR byte string framing (draft-ietf-dtn-mtcpcl-01).
    /// Used by D3TN/ud3tn.
    Mtcp,
    /// STCP: 4-byte big-endian u32 length prefix.
    /// Used by ION (actual wire format, not the STCP spec).
    Stcp,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
pub struct Config {
    /// Logging level (default: INFO)
    #[serde(default = "default_log_level", with = "log_level_serde")]
    pub log_level: Level,

    /// The address of the BPA gRPC server (e.g. "http://[::1]:50051")
    pub bpa_address: String,

    /// The name of this CLA instance to register with the BPA
    pub cla_name: String,

    /// CLA-specific configuration
    #[serde(flatten)]
    pub cla: ClaConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ClaConfig {
    /// TCP address to listen on (e.g., "[::]:4557").
    /// If not set, the CLA will not accept incoming connections.
    pub address: Option<SocketAddr>,

    /// Framing mode: "mtcp" (CBOR byte string) or "stcp" (4-byte u32).
    pub framing: Framing,

    /// Maximum bundle size to accept (bytes). Default: 1GB.
    #[serde(default = "default_max_bundle_size")]
    pub max_bundle_size: u64,

    /// Peer address for outbound connections (e.g., "127.0.0.1:4557").
    pub peer: Option<String>,

    /// Peer node ID (e.g., "ipn:2.0").
    /// When set with `peer`, the CLA calls sink.add_peer() on registration.
    pub peer_node: Option<String>,
}

impl Config {
    pub fn load(config_file: Option<PathBuf>) -> anyhow::Result<Config> {
        let config_file = config_file
            .map(|p| p.to_string_lossy().into_owned())
            .or_else(|| std::env::var("MTCP_CLA_CONFIG_FILE").ok())
            .unwrap_or_else(|| "mtcp-cla".to_string());

        let source_file = config::File::with_name(&config_file);
        let source_env = config::Environment::with_prefix("MTCP_CLA")
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
