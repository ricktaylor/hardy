use super::*;

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default, rename_all = "kebab-case"))]
pub struct Config {
    pub status_reports: bool,
    pub poll_channel_depth: core::num::NonZeroUsize,
    pub processing_pool_size: core::num::NonZeroUsize,
    pub lru_capacity: core::num::NonZeroUsize,
    pub max_cached_bundle_size: core::num::NonZeroUsize,
    pub node_ids: node_ids::NodeIds,
    /// BP-ARP configuration for automatic EID resolution of Neighbours.
    pub arp: cla::arp::ArpConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            status_reports: false,
            poll_channel_depth: core::num::NonZeroUsize::new(16).unwrap(),
            processing_pool_size: core::num::NonZeroUsize::new(
                hardy_async::available_parallelism().get() * 4,
            )
            .unwrap(),
            lru_capacity: core::num::NonZeroUsize::new(1024).unwrap(),
            max_cached_bundle_size: core::num::NonZeroUsize::new(16 * 1024).unwrap(),
            node_ids: node_ids::NodeIds::default(),
            arp: cla::arp::ArpConfig::default(),
        }
    }
}

impl core::fmt::Debug for Config {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Config")
            .field("status_reports", &self.status_reports)
            .field("node_ids", &self.node_ids)
            .field("arp", &self.arp)
            .finish_non_exhaustive()
    }
}
