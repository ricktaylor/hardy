use core::num::NonZeroUsize;

use crate::{
    Arc,
    bpa::Bpa,
    cla::{Cla, registry::ClaRegistryBuilder},
    dispatcher::Dispatcher,
    filter::{
        Filter, FilterEngine, Hook,
        slots::{SlotHandle, SlotRegistry, SlotValue},
        validity::BundleValidityFilter,
    },
    keys::KeyProvider,
    node_ids::NodeIds,
    policy::FlowControllerFactory,
    routing::{RibBuilder, RoutingAgent},
    services::{self, Service, registry::ServiceRegistryBuilder},
    storage::{
        BundleMemStorage, BundleStorage, CachedBundleStorage, MetadataMemStorage, MetadataStorage,
        store::Store,
    },
};

/// Builder for constructing a [`Bpa`] with custom configuration, obtained
/// from [`Bpa::builder()`](crate::bpa::Bpa::builder).
///
/// Provides fluent setters for storage backends, processing pool size,
/// node identifiers, bundle cache parameters, and status report generation.
/// Call [`build()`](BpaBuilder::build) to produce the final [`Bpa`].
///
/// Defaults: in-memory storage (never cached), status reports disabled,
/// processing pool = 4x available parallelism. A configured bundle storage
/// is cached unless [`no_cache()`](BpaBuilder::no_cache) is called.
pub struct BpaBuilder {
    status_reports: bool,
    poll_channel_depth: NonZeroUsize,
    processing_pool_size: NonZeroUsize,
    lru_capacity: Option<NonZeroUsize>,
    max_cached_bundle_size: Option<NonZeroUsize>,
    max_bundle_size: Option<NonZeroUsize>,
    cache_disabled: bool,
    node_ids: NodeIds,
    metadata_storage: Option<Arc<dyn MetadataStorage>>,
    bundle_storage: Option<Arc<dyn BundleStorage>>,
    filter_engine: Arc<FilterEngine>,
    key_provider: Arc<dyn KeyProvider>,
    service_registry_builder: ServiceRegistryBuilder,
    cla_registry_builder: ClaRegistryBuilder,
    rib_builder: RibBuilder,
    slot_registry: SlotRegistry,
}

impl BpaBuilder {
    // The one constructor: reachable only through Bpa::builder().
    pub(crate) fn new() -> Self {
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

        Self {
            poll_channel_depth,
            processing_pool_size,
            filter_engine,
            key_provider: Arc::new(crate::keys::NullKeyProvider),
            status_reports: false,
            lru_capacity: None,
            max_cached_bundle_size: None,
            max_bundle_size: None,
            cache_disabled: false,
            node_ids: NodeIds::default(),
            metadata_storage: None,
            bundle_storage: None,
            service_registry_builder: ServiceRegistryBuilder::new(),
            cla_registry_builder: ClaRegistryBuilder::new(),
            rib_builder: RibBuilder::new(),
            slot_registry: SlotRegistry::default(),
        }
    }

    pub fn bundle_storage(mut self, bundle_storage: Arc<dyn BundleStorage>) -> Self {
        self.bundle_storage = Some(bundle_storage);
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

    /// Sets the LRU cache capacity, in entries; unset applies the cache's
    /// own default. Has no effect when no bundle storage is configured:
    /// the default memory store is never cached.
    pub fn lru_capacity(mut self, v: NonZeroUsize) -> Self {
        self.lru_capacity = Some(v);
        self
    }

    /// Sets the largest bundle size eligible for caching, in bytes; unset
    /// applies the cache's own default. Has no effect when no bundle
    /// storage is configured: the default memory store is never cached.
    /// Sets the maximum size of a single reassembled bundle at ingress.
    ///
    /// Streamed dispatch and streamed service origination accumulate
    /// segments until the bundle is complete; this bound stops a runaway or
    /// hostile producer growing BPA memory without limit. Streams exceeding
    /// it are rejected with an error to the producer. Defaults privately at
    /// the point of use.
    pub fn max_bundle_size(mut self, v: NonZeroUsize) -> Self {
        self.max_bundle_size = Some(v);
        self
    }

    pub fn max_cached_bundle_size(mut self, v: NonZeroUsize) -> Self {
        self.max_cached_bundle_size = Some(v);
        self
    }

    /// Disables the bundle storage cache entirely.
    pub fn no_cache(mut self) -> Self {
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
        policy: Option<Arc<dyn FlowControllerFactory>>,
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

    /// Set the key provider for BPSec operations.
    pub fn key_provider(mut self, provider: Arc<dyn KeyProvider>) -> Self {
        self.key_provider = provider;
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

    /// Registers an annotation slot: a stable name plus a typed,
    /// size-bounded value an embedder's filter pair carries with a bundle
    /// from admission to transmission.
    ///
    /// Returns the builder and the slot's typed [`SlotHandle`] — the
    /// capability gating every read and write of the slot; a filter pair
    /// shares state by sharing the handle. `max_size` bounds the encoded
    /// value: larger writes are dropped (with a warning) when the delta is
    /// applied. Registering the same name twice is rejected loudly by
    /// [`build()`](Self::build).
    pub fn annotation_slot<T: SlotValue>(
        mut self,
        name: &str,
        max_size: NonZeroUsize,
    ) -> (Self, SlotHandle<T>) {
        let handle = self.slot_registry.register(name, max_size);
        (self, handle)
    }

    /// Consume the builder and construct the BPA with all registered components.
    pub async fn build(self) -> Result<Bpa, Box<dyn core::error::Error + Send + Sync>> {
        // Freeze the annotation-slot registrations first: a duplicate name
        // is a construction error. The engine swap (C3) threads the frozen
        // table into the dispatcher; until then freezing is the validation.
        let _slot_table = self.slot_registry.freeze()?;

        let metadata_storage = self
            .metadata_storage
            .unwrap_or_else(|| Arc::new(MetadataMemStorage::new(None)));

        let bundle_storage: Arc<dyn BundleStorage> = match self.bundle_storage {
            Some(raw) if !self.cache_disabled => Arc::new(CachedBundleStorage::new(
                raw,
                self.lru_capacity,
                self.max_cached_bundle_size,
            )),
            Some(raw) => raw,
            None => Arc::new(BundleMemStorage::new(None, None)),
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

        let (dispatcher, start_dispatcher) = Dispatcher::new(
            self.status_reports,
            self.poll_channel_depth,
            self.processing_pool_size,
            self.max_bundle_size,
            node_ids.clone(),
            store.clone(),
            rib.clone(),
            self.key_provider,
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

        // Only now start the dispatch-queue consumer: its storage poller
        // recovers persisted DispatchPending bundles immediately, and
        // processing one dereferences the CLA registry wired above.
        start_dispatcher(&dispatcher);

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
