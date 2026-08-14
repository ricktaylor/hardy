use std::sync::Arc;

use hardy_bpa::cla::Cla;
#[cfg(feature = "file-cla")]
use hardy_file_cla::Cla as FileCla;
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

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum ClaType {
    #[cfg(feature = "tcpclv4")]
    #[serde(rename = "tcpclv4")]
    TcpClv4(super::tcpclv4::Config),

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

// Unknown CLA types are tolerated (`Other`, ignored with a warning at
// build) so a config can name extension CLAs this binary was not built
// with, but a known type with a malformed payload must fail loudly. A
// derived untagged fallback cannot tell the two apart: it swallows the
// payload's parse error and ignores the whole entry, so the dispatch on
// `type` is by hand.
impl<'de> Deserialize<'de> for ClaType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut entry = serde_json::Map::deserialize(deserializer)?;
        let cla_type = match entry.remove("type") {
            Some(serde_json::Value::String(cla_type)) => cla_type,
            Some(_) => return Err(serde::de::Error::custom("type must be a string")),
            None => return Err(serde::de::Error::missing_field("type")),
        };
        let config = serde_json::Value::Object(entry);
        match cla_type.as_str() {
            #[cfg(feature = "tcpclv4")]
            "tcpclv4" => serde_json::from_value(config)
                .map(Self::TcpClv4)
                .map_err(serde::de::Error::custom),
            #[cfg(feature = "file-cla")]
            "file-cla" => serde_json::from_value(config)
                .map(Self::File)
                .map_err(serde::de::Error::custom),
            _ => Ok(Self::Other { cla_type, config }),
        }
    }
}

impl Config {
    pub fn build(&self) -> anyhow::Result<Option<Arc<dyn Cla>>> {
        match &self.cla_type {
            #[cfg(feature = "tcpclv4")]
            ClaType::TcpClv4(config) => {
                let cla =
                    Arc::new(config.build().map_err(|e| {
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
