use core::num::{NonZeroU64, NonZeroUsize};

// `eid::Service` is the service number inside an EID, not the service
// trait registered here; it is aliased to the word the code already uses
// for it.
use hardy_bpv7::eid::Service as ServiceId;
use hardy_eid_patterns::EidPattern;

use crate::{
    Arc, HashMap,
    bpa::Bpa,
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
    // The one constructor: reachable only through Bpa::builder().
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

    /// Sets the LRU cache capacity, in entries; unset applies the cache's
    /// own default. Has no effect when no bundle storage is configured:
    /// the default memory store is never cached.
    pub fn lru_capacity(mut self, v: NonZeroUsize) -> Self {
        self.lru_capacity = Some(v);
        self
    }

    /// Sets the maximum size, in bytes, of a single bundle at any ingress
    /// door.
    ///
    /// A received or originated bundle accumulates in memory until it is
    /// stored, so this bound stops a runaway or hostile producer growing BPA
    /// memory without limit. An over-cap or truncated CLA transfer is
    /// answered [`Acceptance::Refused`](crate::cla::Acceptance) (the CLA
    /// withholds its acknowledgement); an over-cap origination fails with a
    /// size error to the producer. A cap beyond the target's
    /// addressable bound (`isize::MAX`, relevant on 32-bit targets) is
    /// clamped to it — the clamped value is both enforced and advertised to
    /// CLAs at registration.
    pub fn max_bundle_size(mut self, v: NonZeroU64) -> Self {
        self.max_bundle_size = Some(v);
        self
    }

    /// Sets the largest bundle size eligible for caching, in bytes; unset
    /// applies the cache's own default. Has no effect when no bundle
    /// storage is configured: the default memory store is never cached.
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

    /// Declares a CLA, built when the BPA is built.
    ///
    /// The BPA settles the factory's
    /// [`Request`](crate::cla::Request) against its own configuration and
    /// builds the CLA from the [`Terms`](crate::cla::Terms); the CLA is
    /// inert until [`Bpa::start`](crate::bpa::Bpa::start) starts it, after
    /// storage recovery. A name already declared fails
    /// [`build`](Self::build), not this call, so declaration stays
    /// chainable.
    pub fn cla<F: Factory + 'static>(self, name: impl Into<String>, factory: F) -> Self {
        self.cla_boxed(name, BoxFactory::new(factory))
    }

    /// Declares a CLA from an erased factory, for an embedder choosing
    /// among factory types of its own.
    ///
    /// [`cla`](Self::cla) is the typed door, and erases for the caller.
    pub fn cla_boxed(mut self, name: impl Into<String>, factory: BoxFactory<'static>) -> Self {
        self.clas.push((name.into(), factory));
        self
    }

    /// Shapes the egress queues feeding the CLA named `name`.
    ///
    /// The policy is the BPA's own half of the attachment, chosen by the
    /// embedder rather than by the CLA, so it applies to a CLA registered
    /// at runtime exactly as it does to one declared here. A name no CLA
    /// ever attaches under shapes nothing, and a CLA not named here runs
    /// unshaped.
    pub fn egress_policy(
        mut self,
        name: impl Into<String>,
        policy: Arc<dyn FlowControllerFactory>,
    ) -> Self {
        self.cla_policies.insert(name.into(), policy);
        self
    }

    /// Declares a payload-only service, built when the BPA is built and
    /// started when the BPA starts.
    ///
    /// `id` names the endpoint's service number, and `None` asks the BPA
    /// to allocate one. The key comes first because the embedder owns the
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

    /// Declares a routing agent, built when the BPA is built.
    ///
    /// The agent is inert until [`Bpa::start`](crate::bpa::Bpa::start)
    /// starts it, after storage recovery, so the routes it installs
    /// cannot race the consistency check. A name already declared fails
    /// [`build`](Self::build), not this call, so declaration stays
    /// chainable.
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
    /// [`routing_agent`](Self::routing_agent) is the typed door, and
    /// erases for the caller.
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

    /// Sets whether the pre-drain gate requires the primary block to carry
    /// integrity protection (a CRC, or BIB coverage). RFC 9171 §4.3.1;
    /// strict by default. Disable for interoperability with peers that add
    /// neither.
    pub fn primary_block_integrity(mut self, enabled: bool) -> Self {
        self.primary_block_integrity = enabled;
        self
    }

    /// Sets whether the pre-drain gate requires a clockless bundle to carry
    /// a Bundle Age block. RFC 9171 §4.4.2; strict by default. Disable for
    /// compatibility with the RFC 9173 Appendix A test vectors.
    pub fn bundle_age_required(mut self, enabled: bool) -> Self {
        self.bundle_age_required = enabled;
        self
    }

    /// Sets the peers that require legacy 2-element IPN EID encoding: a
    /// bundle forwarded to a next hop matching any of `patterns` has its
    /// `Ipn` source and destination re-encoded as `LegacyIpn` in the
    /// per-hop rewrite stage. Wire adaptation only — the stored bundle and
    /// its id are untouched. A bundle carrying a BPSec operation that covers
    /// the primary block, as its target or through its scope (an encrypted
    /// BIB counts whatever its scope, its ciphertext hiding it), cannot
    /// survive the re-encode: it is deleted at such a next hop, not
    /// re-routed, with `UnexpectedSecurityOperation` as the reason for any
    /// deletion report.
    pub fn ipn_legacy_peers(mut self, patterns: Vec<EidPattern>) -> Self {
        self.ipn_legacy_peers = patterns;
        self
    }

    /// Adds a filter pack: its filter registrations are spliced into the
    /// per-hook chains at [`build()`](Self::build), in registration order
    /// within the pack and in `add_filters` call order across packs. The
    /// registrations' diagnostic labels carry the pack's name as a prefix.
    pub fn add_filters(mut self, pack: FilterPack) -> Self {
        self.filter_packs.push(pack);
        self
    }

    /// Consumes the builder and constructs the BPA with every registered
    /// component.
    pub async fn build(self) -> Result<Bpa, Box<dyn core::error::Error + Send + Sync>> {
        // Freeze the filter packs first: the per-hook chains splice in call
        // order, and each input hook's P = the max peek its registrations
        // declare.
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

        // Each declared service is built here, and its delivery queue
        // left undrained until Bpa::start: one running now would race
        // storage recovery.
        let services =
            service::table::Table::new(node_ids.clone(), &dispatcher, rib.clone(), self.services)?;

        // Each declared CLA is built here, and left inert until
        // Bpa::start: a CLA that opened its transport now would race
        // storage recovery.
        let clas = cla::table::Table::new(
            node_ids.clone(),
            &dispatcher,
            rib.clone(),
            store.clone(),
            self.poll_channel_depth.into(),
            self.cla_policies,
            self.clas,
        )?;

        // Each declared routing agent is built here, and installs no
        // route until Bpa::start starts it, after storage recovery.
        let agents = routing::agent::table::Table::new(node_ids, rib.clone(), self.routing_agents)?;

        // TODO: Dispatcher and the CLA table still reference each other:
        // the table holds the dispatcher, and the dispatcher late-binds the
        // table through its Once cell, so readers driven from Rib::start
        // (poll_waiting) rest on a runtime expect rather than the type
        // system. The start closure fixes the wiring order; the cycle
        // itself remains to be dissolved.
        start_dispatcher(clas.clone());

        Ok(Bpa::from_parts(
            store, rib, clas, services, agents, dispatcher,
        ))
    }
}
