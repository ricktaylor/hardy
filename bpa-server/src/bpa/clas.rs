use super::*;
use hardy_bpa::bpa::BpaRegistration;
use serde::{Deserialize, Serialize};

// A configured Convergence Layer Adaptor instance.
#[derive(Debug, Serialize, Deserialize)]
pub struct Cla {
    // Unique name for this CLA instance (used in logs and route references).
    pub name: String,

    // CLA type and its type-specific configuration (flattened from the `type` field).
    #[serde(flatten)]
    pub config: ClaConfig,

    // Optional egress policy applied to bundles sent through this CLA.
    #[serde(default)]
    pub policy: Option<policy::EgressPolicyConfig>,
}

// CLA type discriminator and type-specific configuration.
//
// The `type` field in the config file selects the variant. Recognised types
// depend on enabled features; unrecognised values fall through to `Other`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClaConfig {
    // TCPCLv4 convergence layer (RFC 9174). Requires the `tcpclv4` feature.
    #[cfg(feature = "tcpclv4")]
    #[serde(rename = "tcpclv4")]
    TcpClv4(hardy_tcpclv4::config::Config),

    // File-based CLA for testing. Requires the `file-cla` feature.
    #[cfg(feature = "file-cla")]
    #[serde(rename = "file-cla")]
    File(hardy_file_cla::Config),

    // Any unrecognised `type` value. When `dynamic-plugins` is enabled,
    // this is treated as a path to a plugin shared library and the
    // remaining fields are captured as JSON for the plugin factory.
    // Otherwise it's ignored with a warning.
    #[serde(untagged)]
    Other {
        #[serde(rename = "type")]
        plugin_path: String,
        #[serde(flatten)]
        config: serde_json::Value,
    },
}

// Create and register all configured CLA instances with the BPA.
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
