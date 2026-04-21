use std::sync::Arc;

use hardy_bpa::cla::Cla;
#[cfg(feature = "file-cla")]
use hardy_file_cla::Cla as FileCla;
#[cfg(feature = "tcpclv4")]
use hardy_tcpclv4::Cla as TcpClv4Cla;
use serde::{Deserialize, Serialize};
use tracing::warn;

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub name: String,
    #[serde(flatten)]
    pub cla_type: ClaType,
    #[serde(default)]
    pub policy: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClaType {
    #[cfg(feature = "tcpclv4")]
    #[serde(rename = "tcpclv4")]
    TcpClv4(hardy_tcpclv4::config::Config),

    #[cfg(feature = "file-cla")]
    #[serde(rename = "file-cla")]
    File(hardy_file_cla::Config),

    #[serde(untagged)]
    Other {
        #[serde(rename = "type")]
        cla_type: String,
        #[serde(flatten)]
        config: serde_json::Value,
    },
}

impl Config {
    pub fn build(&self) -> anyhow::Result<Option<Arc<dyn Cla>>> {
        match &self.cla_type {
            #[cfg(feature = "tcpclv4")]
            ClaType::TcpClv4(config) => {
                let cla =
                    Arc::new(TcpClv4Cla::new(config).map_err(|e| {
                        anyhow::anyhow!("Failed to create CLA '{}': {e}", self.name)
                    })?);
                Ok(Some(cla))
            }
            #[cfg(feature = "file-cla")]
            ClaType::File(config) => {
                let cla =
                    Arc::new(FileCla::new(config).map_err(|e| {
                        anyhow::anyhow!("Failed to create CLA '{}': {e}", self.name)
                    })?);
                Ok(Some(cla))
            }
            ClaType::Other {
                cla_type,
                config: _,
            } => {
                warn!(
                    "Ignoring CLA '{}' with unknown type '{cla_type}'",
                    self.name
                );
                Ok(None)
            }
        }
    }
}
