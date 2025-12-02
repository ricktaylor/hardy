use super::*;
use serde::{Deserialize, Serialize};

/// The EgressPolicy enum separates the type from its configuration.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", content = "config")]
pub enum EgressPolicyConfig {
    // #[cfg(feature = "htb_policy")]
    // Htb { config: hardy_bpa::policy::htb_policy::HtbConfig },

    // #[cfg(feature = "tbf_policy")]
    // Tbf { config: hardy_bpa::policy::tbf_policy::TbfConfig },

    // Catch unknown values
    #[serde(other)]
    Unknown,
}

pub async fn init(
    cla_name: &str,
    config: EgressPolicyConfig,
) -> anyhow::Result<Option<Arc<dyn hardy_bpa::policy::EgressPolicy>>> {
    match config {
        // #[cfg(feature = "htb_policy")]
        // EgressPolicyConfig::Htb { config } => {
        //     let policy = Arc::new(hardy_bpa::policy::htb_policy::HtbPolicy::new(config));
        //     Ok(policy)
        // }
        EgressPolicyConfig::Unknown => {
            warn!("Ignoring unknown policy type for CLA: {cla_name}");
            Ok(None)
        }
    }
}
