use super::*;
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

    // Catch unknown values
    #[serde(other)]
    Unknown,
}

pub async fn init(config: Vec<Cla>, bpa: &Arc<hardy_bpa::bpa::Bpa>) -> anyhow::Result<()> {
    for cla_config in config {
        let policy = if let Some(p) = cla_config.policy {
            policy::init(&cla_config.name, p).await?
        } else {
            None
        };

        match cla_config.config {
            ClaConfig::Unknown => {
                warn!("Ignoring unknown CLA type for CLA: {}", cla_config.name);
            }
            #[cfg(feature = "tcpclv4")]
            ClaConfig::TcpClv4(config) => {
                let cla = Arc::new(hardy_tcpclv4::Cla::new(cla_config.name.clone(), config));

                bpa.register_cla(
                    cla_config.name.clone(),
                    Some(hardy_bpa::cla::ClaAddressType::Tcp),
                    cla,
                    policy,
                )
                .await
                .map_err(|e| anyhow::anyhow!("Failed to start CLA '{}': {e}", cla_config.name))?;

                // TODO: Resolver...
            }
            #[cfg(feature = "file-cla")]
            ClaConfig::File(config) => {
                let cla = Arc::new(hardy_file_cla::Cla::new(&config).map_err(|e| {
                    anyhow::anyhow!("Failed to create CLA '{}': {e}", cla_config.name)
                })?);

                cla.register(bpa, cla_config.name.clone())
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!("Failed to start CLA '{}': {e}", cla_config.name)
                    })?;
            }
        };
    }
    Ok(())
}
