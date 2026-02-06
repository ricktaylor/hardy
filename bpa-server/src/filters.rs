use super::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct Filter {
    pub name: String,
    pub hook: hardy_bpa::filters::Hook,

    #[serde(default)]
    pub after: Vec<String>,

    #[serde(flatten)]
    pub config: FilterConfig,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
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
        let after = filter_config
            .after
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>();

        match filter_config.config {
            FilterConfig::Unknown => {
                warn!(
                    "Ignoring unknown filter type for filter: {}",
                    filter_config.name
                );
            }
            #[cfg(feature = "ipn-legacy-filter")]
            FilterConfig::IpnLegacy(config) => {
                let filter = hardy_bpa::filters::ipn_legacy::init(config);

                bpa.register_filter(
                    filter_config.hook,
                    filter_config.name.as_str(),
                    &after,
                    filter,
                )?;
            }
        };
    }
    Ok(())
}
