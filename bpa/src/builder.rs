use core::num::NonZeroUsize;

use crate::Arc;
use crate::bpa::Bpa;
use crate::cla::Cla;
use crate::cla::registry::ClaRegistryBuilder;
use crate::dispatcher::Dispatcher;
use crate::filter::validity::BundleValidityFilter;
use crate::filter::{Filter, FilterEngine, Hook};
use crate::keys::registry::Registry as KeyRegistry;
use crate::node_ids::NodeIds;
use crate::policy::EgressPolicy;
use crate::rib::RibBuilder;
use crate::routes::RoutingAgent;
use crate::services::registry::ServiceRegistryBuilder;
use crate::services::{self, Service};
use crate::storage::{
    BundleMemStorage, BundleStorage, CachedBundleStorage, DEFAULT_MAX_CACHED_BUNDLE_SIZE,
    MetadataMemStorage, MetadataStorage, Store,
};

/// Builder for constructing a [`Bpa`] with custom configuration.
///
/// Provides fluent setters for storage backends, processing pool size,
/// node identifiers, bundle cache parameters, and status report generation.
/// Call [`build()`](BpaBuilder::build) to produce the final [`Bpa`].
///
/// Defaults: in-memory storage, no LRU cache, status reports disabled,
/// processing pool = 4x available parallelism.
pub struct BpaBuilder {
    status_reports: bool,
    poll_channel_depth: NonZeroUsize,
    processing_pool_size: NonZeroUsize,
    lru_capacity: Option<NonZeroUsize>,
    max_cached_bundle_size: NonZeroUsize,
    cache_disabled: bool,
    node_ids: NodeIds,
    metadata_storage: Option<Arc<dyn MetadataStorage>>,
    bundle_storage: Option<Arc<dyn BundleStorage>>,
    filter_engine: Arc<FilterEngine>,
    keys_registry: Arc<KeyRegistry>,
    service_registry_builder: ServiceRegistryBuilder,
    cla_registry_builder: ClaRegistryBuilder,
    rib_builder: RibBuilder,
}

impl BpaBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bundle_storage(mut self, bundle_storage: Arc<dyn BundleStorage>) -> Self {
        self.bundle_storage = Some(bundle_storage);
        // Auto-enable cache for non-default (presumably persistent) storage,
        // unless the caller has explicitly disabled caching.
        if !self.cache_disabled {
            self.lru_capacity
                .get_or_insert(crate::storage::DEFAULT_LRU_CAPACITY);
        }
        self
    }

    pub fn metadata_storage(mut self, metadata_storage: Arc<dyn MetadataStorage>) -> Self {
        self.metadata_storage = Some(metadata_storage);
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
        self.lru_capacity = Some(v);
        self
    }

    pub fn max_cached_bundle_size(mut self, v: NonZeroUsize) -> Self {
        self.max_cached_bundle_size = v;
        self
    }

    pub fn no_cache(mut self) -> Self {
        self.lru_capacity = None;
        self.cache_disabled = true;
        self
    }

    pub fn node_ids(mut self, v: NodeIds) -> Self {
        self.node_ids = v;
        self
    }

    pub fn service_priority(mut self, priority: u32) -> Self {
        self.rib_builder.service_priority(priority);
        self
    }

    /// Register a CLA to be initialized when the BPA is built.
    pub fn cla(
        mut self,
        name: impl Into<String>,
        cla: Arc<dyn Cla>,
        policy: Option<Arc<dyn EgressPolicy>>,
    ) -> Self {
        self.cla_registry_builder
            .insert(name.into(), cla, policy)
            .expect("Failed to insert CLA");
        self
    }

    /// Register a service to be initialized when the BPA is built.
    pub fn service(
        mut self,
        service: Arc<dyn Service>,
        service_id: hardy_bpv7::eid::Service,
    ) -> Self {
        self.service_registry_builder
            .insert(
                service_id,
                services::registry::ServiceImpl::LowLevel(service),
            )
            .expect("Failed to register service");
        self
    }

    /// Register a routing agent to be initialized when the BPA is built.
    pub fn routing_agent(mut self, name: impl Into<String>, agent: Arc<dyn RoutingAgent>) -> Self {
        self.rib_builder.insert(name.into(), agent);
        self
    }

    /// Register a filter immediately.
    pub fn filter(
        self,
        hook: Hook,
        name: impl Into<String>,
        after: &[&str],
        filter: Filter,
    ) -> Self {
        self.filter_engine
            .register(hook, &name.into(), after, filter)
            .expect("Failed to register filter");
        self
    }

    /// Consume the builder and construct the BPA with all registered components.
    pub async fn build(self) -> Result<Bpa, Box<dyn std::error::Error + Send + Sync>> {
        let metadata_storage = self
            .metadata_storage
            .unwrap_or_else(|| Arc::new(MetadataMemStorage::new(&Default::default())));

        let bundle_storage = {
            let raw = self
                .bundle_storage
                .unwrap_or_else(|| Arc::new(BundleMemStorage::new(&Default::default())));
            match self.lru_capacity {
                Some(capacity) => Arc::new(CachedBundleStorage::new(
                    raw,
                    capacity,
                    self.max_cached_bundle_size,
                )),
                None => raw,
            }
        };

        let store = Arc::new(Store::new(
            self.poll_channel_depth,
            metadata_storage,
            bundle_storage,
        ));

        let node_ids = Arc::new(self.node_ids);
        let rib = self
            .rib_builder
            .build(node_ids.clone(), store.clone())
            .await?;
        let filter_engine = self.filter_engine;
        let keys_registry = self.keys_registry;

        let dispatcher = Dispatcher::new(
            self.status_reports,
            self.poll_channel_depth,
            self.processing_pool_size,
            node_ids.clone(),
            store.clone(),
            rib.clone(),
            keys_registry,
            filter_engine.clone(),
        );

        let (service_registry, cla_registry) = futures::join!(
            self.service_registry_builder
                .build(&node_ids, &rib, &dispatcher),
            self.cla_registry_builder.build(
                &node_ids,
                self.poll_channel_depth.into(),
                &rib,
                &store,
                &dispatcher,
            ),
        );
        let service_registry = service_registry?;
        let cla_registry = cla_registry?;

        // TODO: Remove this circular dependency between Dispatcher and ClaRegistry
        dispatcher.set_cla_registry(cla_registry.clone());

        Ok(Bpa::from_parts(
            node_ids,
            store,
            rib,
            cla_registry,
            service_registry,
            filter_engine,
            dispatcher,
        ))
    }
}

impl Default for BpaBuilder {
    fn default() -> Self {
        let filter_engine = Arc::new(FilterEngine::new());

        // Auto-register bundle validity filter (lifetime, hop-count)
        let validity = Arc::new(BundleValidityFilter);
        filter_engine
            .register(
                Hook::Ingress,
                "bundle-validity",
                &[],
                Filter::Read(validity.clone()),
            )
            .expect("Failed to register bundle validity filter");
        filter_engine
            .register(
                Hook::Originate,
                "bundle-validity",
                &[],
                Filter::Read(validity),
            )
            .expect("Failed to register bundle validity filter");

        // Auto-register RFC9171 validity filter unless disabled
        #[cfg(not(feature = "no-rfc9171-autoregister"))]
        {
            use crate::filter::rfc9171::Rfc9171ValidityFilter;

            filter_engine
                .register(
                    Hook::Ingress,
                    "rfc9171-validity",
                    &[],
                    Filter::Read(Arc::new(Rfc9171ValidityFilter::default())),
                )
                .expect("Failed to register RFC9171 validity filter");
        }

        let poll_channel_depth = NonZeroUsize::new(16).unwrap();
        let processing_pool_size =
            NonZeroUsize::new(hardy_async::available_parallelism().get() * 4).unwrap();
        let keys_registry = Arc::new(KeyRegistry::new());

        Self {
            poll_channel_depth,
            processing_pool_size,
            filter_engine,
            keys_registry,
            status_reports: false,
            lru_capacity: None,
            max_cached_bundle_size: DEFAULT_MAX_CACHED_BUNDLE_SIZE,
            cache_disabled: false,
            node_ids: NodeIds::default(),
            metadata_storage: None,
            bundle_storage: None,
            service_registry_builder: ServiceRegistryBuilder::new(),
            cla_registry_builder: ClaRegistryBuilder::new(),
            rib_builder: RibBuilder::new(),
        }
    }
}
