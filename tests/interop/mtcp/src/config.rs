use serde::{Deserialize, Deserializer};
use std::net::SocketAddr;
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

/// Framing mode for the CLA.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Framing {
    /// MTCP: CBOR byte string framing (draft-ietf-dtn-mtcpcl-01).
    /// Used by D3TN/ud3tn.
    Mtcp,
    /// STCP: 4-byte big-endian u32 length prefix.
    /// Used by ION (actual wire format, not the STCP spec).
    Stcp,
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

    /// CLA-specific configuration
    #[serde(flatten)]
    pub cla: ClaConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ClaConfig {
    /// TCP address to listen on (e.g., "[::]:4556").
    /// If not set, the CLA will not accept incoming connections.
    pub address: Option<SocketAddr>,

    /// Framing mode: "mtcp" (CBOR byte string) or "stcp" (4-byte u32).
    pub framing: Framing,

    /// Maximum bundle size to accept (bytes). Default: 1GB.
    #[serde(default = "default_max_bundle_size")]
    pub max_bundle_size: u64,

    /// Peer address for outbound connections (e.g., "127.0.0.1:4556").
    pub peer: Option<String>,

    /// Peer node ID (e.g., "ipn:2.0").
    /// When set with `peer`, the CLA calls sink.add_peer() on registration.
    pub peer_node: Option<String>,
}

fn default_max_bundle_size() -> u64 {
    0x4000_0000 // 1GB
}

pub fn load(path: Option<PathBuf>) -> anyhow::Result<Config> {
    let mut builder = config::Config::builder();

    if let Some(path) = path {
        builder = builder.add_source(config::File::from(path));
    } else {
        builder = builder
            .add_source(config::File::from(std::path::Path::new("mtcp-cla.toml")).required(false));
    }

    builder = builder.add_source(config::Environment::with_prefix("MTCP_CLA"));

    builder.build()?.try_deserialize().map_err(Into::into)
}
