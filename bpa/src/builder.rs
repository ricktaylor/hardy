use core::num::NonZeroUsize;

use crate::Arc;
use crate::bpa::Bpa;
use crate::cla::registry::Registry as ClaRegistry;
use crate::dispatcher::Dispatcher;
use crate::filters::registry::Registry as FilterRegistry;
use crate::keys::registry::Registry as KeyRegistry;
use crate::node_ids::NodeIds;
use crate::rib::Rib;
use crate::services::registry::Registry as ServiceRegistry;
use crate::storage::bundle_mem::BundleMemStorage;
use crate::storage::metadata_mem::MetadataMemStorage;
use crate::storage::{BundleStorage, MetadataStorage, Store};

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
    pub fn new() -> Self {
        Self::default()
    }

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
        let store = Arc::new(Store::new(
            self.lru_capacity,
            self.max_cached_bundle_size,
            self.poll_channel_depth,
            self.metadata_storage,
            self.bundle_storage,
        ));

        let rib = Arc::new(Rib::new(self.node_ids.clone(), store.clone()));

        let cla_registry = Arc::new(ClaRegistry::new(
            (&self.node_ids).into(),
            self.poll_channel_depth.into(),
            rib.clone(),
            store.clone(),
        ));
        let keys_registry = Arc::new(KeyRegistry::new());
        let service_registry = Arc::new(ServiceRegistry::new(self.node_ids.clone(), rib.clone()));
        let filter_registry = Arc::new(FilterRegistry::new());

        // Auto-register RFC9171 validity filter unless disabled
        #[cfg(not(feature = "no-rfc9171-autoregister"))]
        {
            use crate::filters::rfc9171::Rfc9171ValidityFilter;

            filter_registry
                .register(
                    Hook::Ingress,
                    "rfc9171-validity",
                    &[],
                    Filter::Read(Arc::new(Rfc9171ValidityFilter::default())),
                )
                .expect("Failed to register RFC9171 validity filter");
        }

        let dispatcher = Dispatcher::new(
            self.status_reports,
            self.poll_channel_depth,
            self.processing_pool_size,
            self.node_ids,
            store.clone(),
            cla_registry.clone(),
            rib.clone(),
            keys_registry,
            filter_registry.clone(),
        );

        Bpa::from_parts(
            store,
            rib,
            cla_registry,
            service_registry,
            filter_registry,
            dispatcher,
        )
    }
}

impl Default for BpaBuilder {
    fn default() -> Self {
        let status_reports = false;
        let poll_channel_depth = NonZeroUsize::new(16).unwrap();
        let processing_pool_size =
            NonZeroUsize::new(hardy_async::available_parallelism().get() * 4).unwrap();
        let lru_capacity = NonZeroUsize::new(1024).unwrap();
        let max_cached_bundle_size = NonZeroUsize::new(16 * 1024).unwrap();
        let node_ids = NodeIds::default();
        let metadata_storage = Arc::new(MetadataMemStorage::new(&Default::default()));
        let bundle_storage = Arc::new(BundleMemStorage::new(&Default::default()));

        Self {
            status_reports,
            poll_channel_depth,
            processing_pool_size,
            lru_capacity,
            max_cached_bundle_size,
            node_ids,
            metadata_storage,
            bundle_storage,
        }
    }
}
