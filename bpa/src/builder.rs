use core::num::{NonZeroU64, NonZeroUsize};

// `eid::Service` is a service number, not the service trait registered
// here, so it is aliased to the word the rest of the code uses for it.
use hardy_bpv7::eid::Service as ServiceId;
use hardy_eid_patterns::EidPattern;

use crate::{
    Arc, HashMap,
    bpa::{Bpa, Factories},
    cla::{self, BoxFactory, Factory},
    dispatcher::{Config as DispatcherConfig, Dispatcher},
    filter::pack::{FilterPack, chains::FilterChains},
    keys::KeyProvider,
    node_ids::NodeIds,
    policy::FlowControllerFactory,
    routing::{self, Rib},
    service::{self, table::FactoryKind},
    storage::{
        BundleMemStorage, BundleStorage, CachedBundleStorage, MetadataMemStorage, MetadataStorage,
        store::Store,
    },
};

/// Builds a [`Bpa`], obtained from
/// [`Bpa::builder`](crate::bpa::Bpa::builder).
///
/// The setters cover the storage backends, the processing pool, the node
/// identities, the bundle cache, status reports, and the components the
/// BPA declares. [`build`](Self::build) produces the [`Bpa`].
///
/// The defaults are in-memory storage, which is never cached, status
/// reports disabled, and a processing pool of four times the available
/// parallelism. Configured bundle storage is cached unless
/// [`no_cache`](Self::no_cache) is called.
#[must_use = "a builder does nothing until build() is called"]
pub struct BpaBuilder {
    status_reports: bool,
    poll_channel_depth: NonZeroUsize,
    processing_pool_size: NonZeroUsize,
    lru_capacity: Option<NonZeroUsize>,
    max_cached_bundle_size: Option<NonZeroUsize>,
    max_bundle_size: Option<NonZeroU64>,
    cache_disabled: bool,
    node_ids: NodeIds,
    metadata_storage: Option<Arc<dyn MetadataStorage>>,
    bundle_storage: Option<Arc<dyn BundleStorage>>,
    key_provider: Arc<dyn KeyProvider>,
    services: Vec<(Option<ServiceId>, FactoryKind<'static>)>,
    clas: Vec<(String, BoxFactory<'static>)>,
    cla_policies: HashMap<String, Arc<dyn FlowControllerFactory>>,
    routing_agents: Vec<(String, routing::BoxFactory<'static>)>,
    service_priority: u32,
    filter_packs: Vec<FilterPack>,
    primary_block_integrity: bool,
    bundle_age_required: bool,
    ipn_legacy_peers: Vec<EidPattern>,
}

impl BpaBuilder {
    pub(crate) fn new() -> Self {
        let poll_channel_depth = NonZeroUsize::new(16).unwrap();
        let processing_pool_size =
            NonZeroUsize::new(hardy_async::available_parallelism().get() * 4).unwrap();

        Self {
            poll_channel_depth,
            processing_pool_size,
            key_provider: Arc::new(crate::keys::NullKeyProvider),
            status_reports: false,
            lru_capacity: None,
            max_cached_bundle_size: None,
            max_bundle_size: None,
            cache_disabled: false,
            node_ids: NodeIds::default(),
            metadata_storage: None,
            bundle_storage: None,
            services: Vec::new(),
            clas: Vec::new(),
            cla_policies: HashMap::new(),
            routing_agents: Vec::new(),
            service_priority: 1,
            filter_packs: Vec::new(),
            primary_block_integrity: true,
            bundle_age_required: true,
            ipn_legacy_peers: Vec::new(),
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

    /// Sets the LRU cache capacity, in entries. Unset, the cache's own
    /// default applies. This has no effect where no bundle storage is
    /// configured, since the default memory store is never cached.
    pub fn lru_capacity(mut self, v: NonZeroUsize) -> Self {
        self.lru_capacity = Some(v);
        self
    }

    /// Sets the maximum size, in bytes, of a single bundle entering the
    /// BPA.
    ///
    /// A received or originated bundle accumulates in memory until it is
    /// stored, so this bound stops a runaway or hostile producer from
    /// growing BPA memory without limit. An over-cap or truncated CLA
    /// transfer is answered
    /// [`Acceptance::Refused`](crate::cla::Acceptance), so the CLA
    /// withholds its acknowledgement, and an over-cap origination fails
    /// with a size error to the producer.
    ///
    /// A cap beyond the target's addressable bound, `isize::MAX`, is
    /// clamped to it, and the clamped value is both enforced and
    /// advertised to CLAs at registration. Unset, an interim default of
    /// 64 MiB applies, for as long as ingress accumulates a whole bundle
    /// in memory.
    pub fn max_bundle_size(mut self, v: NonZeroU64) -> Self {
        self.max_bundle_size = Some(v);
        self
    }

    /// Sets the largest bundle size eligible for caching, in bytes.
    /// Unset, the cache's own default applies. This has no effect where no
    /// bundle storage is configured, since the default memory store is
    /// never cached.
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
        self.service_priority = priority;
        self
    }

    /// Declares a CLA, registered at
    /// [`Bpa::start`](crate::bpa::Bpa::start).
    ///
    /// The BPA negotiates the factory's [`Request`](crate::cla::Request)
    /// against its own configuration and builds the CLA from the
    /// resulting [`Terms`](crate::cla::Terms). A name already declared, or
    /// a CLA that cannot run, fails `start` rather than this call, so that
    /// declaration stays chainable.
    pub fn cla<F: Factory + 'static>(self, name: impl Into<String>, factory: F) -> Self {
        self.cla_boxed(name, BoxFactory::new(factory))
    }

    /// Declares a CLA from an erased factory, for an embedder choosing
    /// among factory types of its own.
    ///
    /// [`cla`](Self::cla) takes a typed factory and erases it for the
    /// caller.
    pub fn cla_boxed(mut self, name: impl Into<String>, factory: BoxFactory<'static>) -> Self {
        self.clas.push((name.into(), factory));
        self
    }

    /// Shapes the egress queues feeding the CLA named `name`.
    ///
    /// The policy is the embedder's choice rather than the CLA's, so it
    /// applies to a CLA registered while the BPA runs exactly as it does
    /// to one declared here. A name no CLA ever registers under shapes
    /// nothing, and a CLA not named here runs unshaped.
    pub fn egress_policy(
        mut self,
        name: impl Into<String>,
        policy: Arc<dyn FlowControllerFactory>,
    ) -> Self {
        self.cla_policies.insert(name.into(), policy);
        self
    }

    /// Declares a payload-only service, registered at
    /// [`Bpa::start`](crate::bpa::Bpa::start).
    ///
    /// `id` names the endpoint's service number, and `None` asks the BPA
    /// to allocate one. The id comes first because the embedder owns the
    /// namespace: two instances of the same service crate are told apart
    /// here, without the crate's cooperation.
    pub fn service<F: service::Factory + 'static>(
        mut self,
        id: Option<ServiceId>,
        factory: F,
    ) -> Self {
        self.services
            .push((id, FactoryKind::Payload(service::BoxFactory::new(factory))));
        self
    }

    /// Declares a whole-bundle service, as [`service`](Self::service)
    /// does.
    pub fn raw_service<F: service::raw::Factory + 'static>(
        mut self,
        id: Option<ServiceId>,
        factory: F,
    ) -> Self {
        self.services
            .push((id, FactoryKind::Raw(service::raw::BoxFactory::new(factory))));
        self
    }

    /// Declares a routing agent, registered at
    /// [`Bpa::start`](crate::bpa::Bpa::start), after storage recovery,
    /// so the routes it installs cannot race the consistency check.
    ///
    /// A name already declared, or an agent that cannot run, fails
    /// `start` rather than this call, so declaration stays chainable.
    pub fn routing_agent<F: routing::Factory + 'static>(
        self,
        name: impl Into<String>,
        factory: F,
    ) -> Self {
        self.routing_agent_boxed(name, routing::BoxFactory::new(factory))
    }

    /// Declares a routing agent from an erased factory, for an embedder
    /// choosing among factory types of its own.
    ///
    /// [`routing_agent`](Self::routing_agent) takes a typed factory and
    /// erases it for the caller.
    pub fn routing_agent_boxed(
        mut self,
        name: impl Into<String>,
        factory: routing::BoxFactory<'static>,
    ) -> Self {
        self.routing_agents.push((name.into(), factory));
        self
    }

    /// Sets the key provider for BPSec operations.
    pub fn key_provider(mut self, provider: Arc<dyn KeyProvider>) -> Self {
        self.key_provider = provider;
        self
    }

    /// Sets whether a received bundle must carry integrity protection over
    /// its primary block, a CRC or BIB coverage (RFC 9171 Section 4.3.1).
    ///
    /// Required by default. Disable it for interoperability with peers
    /// that send neither.
    pub fn primary_block_integrity(mut self, enabled: bool) -> Self {
        self.primary_block_integrity = enabled;
        self
    }

    /// Sets whether a received bundle whose creation timestamp carries no
    /// clock must contain a Bundle Age block (RFC 9171 Section 4.4.2).
    ///
    /// Required by default. Disable it for compatibility with the RFC 9173
    /// Appendix A test vectors.
    pub fn bundle_age_required(mut self, enabled: bool) -> Self {
        self.bundle_age_required = enabled;
        self
    }

    /// Sets the peers that require legacy 2-element IPN EID encoding.
    ///
    /// A bundle forwarded to a next hop matching any of `patterns` has its
    /// `Ipn` source and destination re-encoded as `LegacyIpn` when the
    /// bundle is rewritten for that hop. The adaptation is to the wire form only; the
    /// stored bundle and its id are untouched.
    ///
    /// A bundle carrying a BPSec operation that covers the primary block,
    /// as its target or through its scope, cannot survive the re-encode.
    /// It is deleted at such a next hop rather than re-routed, with
    /// `UnexpectedSecurityOperation` as the reason for any deletion
    /// report. An encrypted BIB counts whatever its scope, since its
    /// ciphertext hides it.
    pub fn ipn_legacy_peers(mut self, patterns: Vec<EidPattern>) -> Self {
        self.ipn_legacy_peers = patterns;
        self
    }

    /// Adds a filter pack.
    ///
    /// Its filter registrations are spliced into the per-hook chains at
    /// [`build`](Self::build), in registration order within the pack and
    /// in `add_filters` call order across packs. The registrations'
    /// diagnostic labels carry the pack's name as a prefix.
    pub fn add_filters(mut self, pack: FilterPack) -> Self {
        self.filter_packs.push(pack);
        self
    }

    /// Consumes the builder and constructs the BPA, whose declared
    /// components register at [`Bpa::start`](crate::bpa::Bpa::start).
    ///
    /// The BPA returned is inert: no component exists, and no task runs,
    /// until it starts.
    pub fn build(self) -> Bpa {
        // Freeze the filter packs first: the per-hook chains splice in
        // call order, and each input hook's peek is the largest its
        // registrations declare.
        let filter_chains = FilterChains::freeze(self.filter_packs);

        let metadata_storage = self
            .metadata_storage
            .unwrap_or_else(|| Arc::new(MetadataMemStorage::new(None)));

        let bundle_storage: Arc<dyn BundleStorage> = match self.bundle_storage {
            Some(backend) if !self.cache_disabled => Arc::new(CachedBundleStorage::new(
                backend,
                self.lru_capacity,
                self.max_cached_bundle_size,
            )),
            Some(backend) => backend,
            None => Arc::new(BundleMemStorage::new(None, None)),
        };

        let store = Arc::new(Store::new(
            self.poll_channel_depth,
            metadata_storage,
            bundle_storage,
        ));

        let node_ids = Arc::new(self.node_ids);
        let rib = Arc::new(Rib::new(
            node_ids.clone(),
            store.clone(),
            self.service_priority,
        ));

        let (dispatcher, start_dispatcher) = Dispatcher::new(
            DispatcherConfig {
                status_reports: self.status_reports,
                poll_channel_depth: self.poll_channel_depth,
                processing_pool_size: self.processing_pool_size,
                max_bundle_size: self.max_bundle_size,
                primary_block_integrity: self.primary_block_integrity,
                bundle_age_required: self.bundle_age_required,
                ipn_legacy_peers: self.ipn_legacy_peers,
            },
            node_ids.clone(),
            store.clone(),
            rib.clone(),
            self.key_provider,
            filter_chains,
        );

        // The tables start empty: a declared component registers at
        // `Bpa::start`, by the same path as one registered while the BPA
        // runs.
        let services = service::table::Table::new(node_ids.clone(), &dispatcher, rib.clone());
        let clas = cla::table::Table::new(
            node_ids.clone(),
            &dispatcher,
            rib.clone(),
            store.clone(),
            self.poll_channel_depth.into(),
            self.cla_policies,
        );
        let agents = routing::agent::table::Table::new(node_ids, rib.clone());

        // TODO: the dispatcher and the CLA table still reference each
        // other. The table holds the dispatcher, and the dispatcher
        // late-binds the table through a `Once` cell, so the readers driven
        // from `Rib::start` rest on a runtime `expect` rather than on the
        // type system. The start closure fixes the wiring order; the cycle
        // itself remains.
        start_dispatcher(clas.clone());

        Bpa::from_parts(
            store,
            rib,
            clas,
            services,
            agents,
            dispatcher,
            Factories {
                clas: self.clas,
                services: self.services,
                agents: self.routing_agents,
            },
        )
    }
}
