use core::num::NonZeroUsize;

use crate::storage::{BundleMemStorage, BundleStorage, MetadataMemStorage, MetadataStorage};
use crate::{Arc, Bpa, NodeIds};

pub struct BpaBuilder {
    status_reports: bool,
    poll_channel_depth: NonZeroUsize,
    processing_pool_size: NonZeroUsize,
    lru_capacity: NonZeroUsize,
    max_cached_bundle_size: NonZeroUsize,
    node_ids: NodeIds,
    metadata_storage: Arc<dyn MetadataStorage>,
    bundle_storage: Arc<dyn BundleStorage>,
}

impl BpaBuilder {
    pub fn bundle_storage(mut self, bundle_storage: Arc<dyn BundleStorage>) -> Self {
        self.bundle_storage = bundle_storage;
        self
    }

    pub fn metadata_storage(mut self, metadata_storage: Arc<dyn MetadataStorage>) -> Self {
        self.metadata_storage = metadata_storage;
        self
    }

    pub fn status_reports(mut self, v: bool) -> Self {
        self.status_reports = v;
        self
    }

    pub fn poll_channel_depth(mut self, v: NonZeroUsize) -> Self {
        self.poll_channel_depth = v;
        self
    }

    pub fn processing_pool_size(mut self, v: NonZeroUsize) -> Self {
        self.processing_pool_size = v;
        self
    }

    pub fn lru_capacity(mut self, v: NonZeroUsize) -> Self {
        self.lru_capacity = v;
        self
    }

    pub fn max_cached_bundle_size(mut self, v: NonZeroUsize) -> Self {
        self.max_cached_bundle_size = v;
        self
    }

    pub fn node_ids(mut self, v: NodeIds) -> Self {
        self.node_ids = v;
        self
    }

    pub fn build(self) -> Bpa {
        Bpa::new(
            self.status_reports,
            self.poll_channel_depth,
            self.processing_pool_size,
            self.lru_capacity,
            self.max_cached_bundle_size,
            self.node_ids,
            self.metadata_storage,
            self.bundle_storage,
        )
    }
}

impl Default for BpaBuilder {
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
            node_ids: NodeIds::default(),
            metadata_storage: Arc::new(MetadataMemStorage::new(&Default::default())),
            bundle_storage: Arc::new(BundleMemStorage::new(&Default::default())),
        }
    }
}
