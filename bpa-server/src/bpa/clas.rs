use super::*;
use hardy_bpa::bpa::BpaRegistration;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct Cla {
    pub name: String,

    #[serde(flatten)]
    pub config: ClaConfig,

    #[serde(default)]
    pub policy: Option<policy::EgressPolicyConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClaConfig {
    #[cfg(feature = "tcpclv4")]
    #[serde(rename = "tcpclv4")]
    TcpClv4(hardy_tcpclv4::config::Config),

    #[cfg(feature = "file-cla")]
    #[serde(rename = "file-cla")]
    File(hardy_file_cla::Config),

    /// Any unrecognised `type` value. When `dynamic-plugins` is enabled,
    /// this is treated as a path to a plugin shared library and the
    /// remaining fields are captured as JSON for the plugin factory.
    /// Otherwise it's ignored with a warning.
    #[serde(untagged)]
    Other {
        #[serde(rename = "type")]
        plugin_path: String,
        #[serde(flatten)]
        config: serde_json::Value,
    },
}

#[allow(unused_variables)]
pub async fn init(config: &[Cla], bpa: &dyn BpaRegistration) -> anyhow::Result<()> {
    for cla_config in config {
        let policy = if let Some(p) = &cla_config.policy {
            policy::init(&cla_config.name, p).await?
        } else {
            None
        };

        match &cla_config.config {
            #[cfg(feature = "tcpclv4")]
            ClaConfig::TcpClv4(config) => {
                let cla = Arc::new(hardy_tcpclv4::Cla::new(config).map_err(|e| {
                    anyhow::anyhow!("Failed to create CLA '{}': {e}", cla_config.name)
                })?);

                cla.register(bpa, cla_config.name.clone(), policy)
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!("Failed to start CLA '{}': {e}", cla_config.name)
                    })?;
            }
            #[cfg(feature = "file-cla")]
            ClaConfig::File(config) => {
                let cla = Arc::new(hardy_file_cla::Cla::new(config).map_err(|e| {
                    anyhow::anyhow!("Failed to create CLA '{}': {e}", cla_config.name)
                })?);

                cla.register(bpa, cla_config.name.clone())
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!("Failed to start CLA '{}': {e}", cla_config.name)
                    })?;
            }
            ClaConfig::Other {
                plugin_path,
                config,
            } => {
                warn!(
                    "Ignoring CLA '{}' with unknown type '{}'",
                    cla_config.name, plugin_path
                );
            }
        };
    }
    Ok(())
}
