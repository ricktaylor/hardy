use super::*;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub status_reports: bool,
    pub wait_sample_interval: time::Duration,
    pub max_forwarding_delay: u32,

    #[serde(skip)]
    pub metadata_storage: Option<Arc<dyn storage::MetadataStorage>>,

    #[serde(skip)]
    pub bundle_storage: Option<Arc<dyn storage::BundleStorage>>,

    pub admin_endpoints: admin_endpoints::AdminEndpoints,
    pub ipn_2_element: Vec<eid_pattern::EidPattern>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            status_reports: false,
            wait_sample_interval: time::Duration::seconds(60),
            max_forwarding_delay: 5,
            metadata_storage: None,
            bundle_storage: None,
            admin_endpoints: admin_endpoints::AdminEndpoints::default(),
            ipn_2_element: Vec::new(),
        }
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("status_reports", &self.status_reports)
            .field("wait_sample_interval", &self.wait_sample_interval)
            .field("max_forwarding_delay", &self.max_forwarding_delay)
            .field("admin_endpoints", &self.admin_endpoints)
            .field("ipn_2_element", &self.ipn_2_element)
            .finish()
    }
}
