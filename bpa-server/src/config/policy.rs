use std::sync::Arc;

use hardy_bpa::policy::EgressPolicy;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", content = "config")]
pub enum EgressPolicyConfig {
    #[serde(other)]
    Unknown,
}

impl EgressPolicyConfig {
    pub fn build(self) -> anyhow::Result<Arc<dyn EgressPolicy>> {
        match self {
            Self::Unknown => Err(anyhow::anyhow!("Unknown policy type")),
        }
    }
}
