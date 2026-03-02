use super::*;

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default, rename_all = "kebab-case"))]
pub struct Config {
    pub status_reports: bool,
    pub poll_channel_depth: core::num::NonZeroUsize,
    pub processing_pool_size: core::num::NonZeroUsize,

    #[cfg_attr(feature = "serde", serde(skip))]
    pub storage: storage::Config,

    pub node_ids: node_ids::NodeIds,
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
            storage: storage::Config::default(),
            node_ids: node_ids::NodeIds::default(),
        }
    }
}

impl core::fmt::Debug for Config {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Config")
            .field("status_reports", &self.status_reports)
            .field("node_ids", &self.node_ids)
            .finish_non_exhaustive()
    }
}
