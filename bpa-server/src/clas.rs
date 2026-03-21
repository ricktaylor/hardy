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

#[cfg(feature = "dynamic-plugins")]
pub struct PluginLibraries(Vec<hardy_plugin_abi::host::Library>);

#[cfg(feature = "dynamic-plugins")]
impl PluginLibraries {
    pub fn new() -> Self {
        Self(Vec::new())
    }
}

#[allow(unused_variables)]
pub async fn init(
    config: &[Cla],
    bpa: &dyn BpaRegistration,
    #[cfg(feature = "dynamic-plugins")] plugin_libs: &mut PluginLibraries,
) -> anyhow::Result<()> {
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
                #[cfg(feature = "dynamic-plugins")]
                {
                    let config_json = serde_json::to_string(config)?;
                    let (lib, cla) = unsafe {
                        hardy_plugin_abi::host::load_cla_plugin(
                            std::path::Path::new(plugin_path),
                            &config_json,
                        )
                    }
                    .map_err(|e| {
                        anyhow::anyhow!("Failed to load CLA plugin '{}': {e}", cla_config.name)
                    })?;

                    bpa.register_cla(cla_config.name.clone(), None, cla, policy)
                        .await
                        .map_err(|e| {
                            anyhow::anyhow!("Failed to register CLA '{}': {e}", cla_config.name)
                        })?;

                    plugin_libs.0.push(lib);
                }

                #[cfg(not(feature = "dynamic-plugins"))]
                {
                    warn!(
                        "Ignoring CLA '{}' with unknown type '{}' \
                         (enable dynamic-plugins feature to load plugins)",
                        cla_config.name, plugin_path
                    );
                }
            }
        };
    }
    Ok(())
}
