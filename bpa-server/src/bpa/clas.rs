use super::*;
use serde::{Deserialize, Serialize};

// A configured Convergence Layer Adaptor instance.
#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    // Unique name for this CLA instance (used in logs and route references).
    pub name: String,

    // CLA type and its type-specific configuration (flattened from the `type` field).
    #[serde(flatten)]
    pub cla_type: ClaType,

    // Optional egress policy applied to bundles sent through this CLA.
    #[serde(default)]
    pub policy: Option<String>,
}

// CLA type discriminator and type-specific configuration.
//
// The `type` field in the config file selects the variant. Recognised types
// depend on enabled features; unrecognised values fall through to `Other`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClaType {
    // TCPCLv4 convergence layer (RFC 9174). Requires the `tcpclv4` feature.
    #[cfg(feature = "tcpclv4")]
    #[serde(rename = "tcpclv4")]
    TcpClv4(hardy_tcpclv4::config::Config),

    // File-based CLA for testing. Requires the `file-cla` feature.
    #[cfg(feature = "file-cla")]
    #[serde(rename = "file-cla")]
    File(hardy_file_cla::Config),

    // Fallback for unrecognised `type` values — logged as a warning.
    #[serde(untagged)]
    Other {
        #[serde(rename = "type")]
        cla_type: String,
        #[serde(flatten)]
        config: serde_json::Value,
    },
}

type NewClaResult = (
    Arc<dyn hardy_bpa::cla::Cla>,
    Option<hardy_bpa::cla::ClaAddressType>,
);

// Create a CLA instance from config. Returns None for unrecognised types.
#[allow(unused_variables)]
pub fn new(name: &str, cla_type: &ClaType) -> anyhow::Result<Option<NewClaResult>> {
    match cla_type {
        #[cfg(feature = "tcpclv4")]
        ClaType::TcpClv4(config) => {
            let cla = Arc::new(
                hardy_tcpclv4::Cla::new(config)
                    .map_err(|e| anyhow::anyhow!("Failed to create CLA '{name}': {e}"))?,
            );
            Ok(Some((cla, Some(hardy_bpa::cla::ClaAddressType::Tcp))))
        }
        #[cfg(feature = "file-cla")]
        ClaType::File(config) => {
            let cla = Arc::new(
                hardy_file_cla::Cla::new(config)
                    .map_err(|e| anyhow::anyhow!("Failed to create CLA '{name}': {e}"))?,
            );
            Ok(Some((cla, None)))
        }
        ClaType::Other { cla_type, config } => {
            warn!("Ignoring CLA '{name}' with unknown type '{cla_type}'");
            Ok(None)
        }
    }
}
