use super::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct Cla {
    pub name: String,

    #[serde(flatten)]
    pub cla: ClaConfig,
    // TODO Policy!!
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "config")]
pub enum ClaConfig {
    #[cfg(feature = "tcpclv4")]
    #[serde(rename = "tcpclv4")]
    TcpClv4(hardy_tcpclv4::config::Config),

    #[cfg(feature = "file-cla")]
    #[serde(rename = "file-cla")]
    File(hardy_file_cla::Config),

    //UdpCl(UdpclConfig),
    //Btpu-Ethernet(BtpuEthernetConfig),

    // Catch unknown values
    #[serde(other)]
    Unknown,
}

pub async fn init(config: Vec<Cla>, bpa: &Arc<hardy_bpa::bpa::Bpa>) -> anyhow::Result<()> {
    for cla_config in config {
        match cla_config.cla {
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
                    None,
                )
                .await
                .map_err(|e| anyhow::anyhow!("Failed to start CLA '{}': {e}", cla_config.name))?;

                // TODO: Resolver...
            }
            #[cfg(feature = "file-cla")]
            ClaConfig::File(config) => {
                let cla = Arc::new(hardy_file_cla::Cla::new(cla_config.name.clone(), config));

                bpa.register_cla(cla_config.name.clone(), None, cla, None)
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!("Failed to start CLA '{}': {e}", cla_config.name)
                    })?;
            }
        };
    }
    Ok(())
}
