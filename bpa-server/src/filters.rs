use super::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct Filter {
    pub name: String,

    #[serde(flatten)]
    pub config: FilterConfig,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "config")]
pub enum FilterConfig {
    #[cfg(feature = "ipn-legacy-filter")]
    #[serde(rename = "ipn-legacy")]
    IpnLegacy(hardy_bpa::filters::ipn_legacy::Config),

    // Catch unknown values
    #[serde(other)]
    Unknown,
}

pub fn init(config: Vec<Filter>, bpa: &Arc<hardy_bpa::bpa::Bpa>) -> anyhow::Result<()> {
    for filter_config in config {
        match filter_config.config {
            FilterConfig::Unknown => {
                warn!(
                    "Ignoring unknown filter type for filter: {}",
                    filter_config.name
                );
            }
            #[cfg(feature = "ipn-legacy-filter")]
            FilterConfig::IpnLegacy(config) => {
                hardy_bpa::filters::ipn_legacy::register_filter(bpa, config)?;
            }
        };
    }
    Ok(())
}
