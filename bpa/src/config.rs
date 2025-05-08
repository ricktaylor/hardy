use super::*;
use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub status_reports: bool,

    #[serde(skip)]
    pub metadata_storage: Option<Arc<dyn storage::MetadataStorage>>,

    #[serde(skip)]
    pub bundle_storage: Option<Arc<dyn storage::BundleStorage>>,

    pub node_ids: node_ids::NodeIds,
    pub ipn_2_element: Vec<eid_pattern::EidPattern>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("status_reports", &self.status_reports)
            .field("node_ids", &self.node_ids)
            .field("ipn_2_element", &self.ipn_2_element)
            .finish()
    }
}
