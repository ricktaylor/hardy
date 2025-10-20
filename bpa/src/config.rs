use super::*;

#[derive(Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct Config {
    pub status_reports: bool,

    #[cfg_attr(feature = "serde", serde(default = "default_poll_channel_depth"))]
    pub poll_channel_depth: usize,

    #[cfg_attr(feature = "serde", serde(default, rename = "storage"))]
    pub storage_config: storage::Config,

    #[cfg_attr(feature = "serde", serde(skip))]
    pub metadata_storage: Option<Arc<dyn storage::MetadataStorage>>,

    #[cfg_attr(feature = "serde", serde(skip))]
    pub bundle_storage: Option<Arc<dyn storage::BundleStorage>>,

    pub node_ids: node_ids::NodeIds,
    pub ipn_2_element: Vec<hardy_eid_pattern::EidPattern>,
}

#[cfg(feature = "serde")]
fn default_poll_channel_depth() -> usize {
    16
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
