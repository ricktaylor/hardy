use alloc::{boxed::Box, string::String, vec::Vec};
use core::num::{NonZeroU64, NonZeroUsize};

use hardy_bpv7::eid::Service as ServiceId;
use hardy_eid_patterns::EidPattern;
use thiserror::Error;

// Aliased: the CLA and service kinds each have an erased `BoxFactory`.
use crate::{
    Arc,
    bpa::Bpa,
    cla::{self, Factory, factory::BoxFactory as BoxClaFactory, registry::ClaRegistryBuilder},
    dispatcher::{Config as DispatcherConfig, Dispatcher},
    filter::pack::{FilterPack, chains::FilterChains},
    keys::KeyProvider,
    node_ids::NodeIds,
    policy::FlowControllerFactory,
    routing::{self, AgentFactory, RibBuilder},
    services::{
        self, ApplicationFactory, ServiceFactory, factory::BoxFactory as BoxServiceFactory,
        registry::ServiceRegistryBuilder,
    },
    storage::{
        BundleMemStorage, BundleStorage, CachedBundleStorage, MetadataMemStorage, MetadataStorage,
        store::Store,
    },
};

/// A specialized `Result` type for [`BpaBuilder::build`].
pub type Result<T> = core::result::Result<T, Error>;

/// Why [`BpaBuilder::build`] failed.
#[derive(Debug, Error)]
pub enum Error {
    /// A configured CLA could not be registered: its name is taken.
    #[error("Failed to register CLA '{name}': {source}")]
    Cla {
        /// The name the CLA was configured under.
        name: String,
        /// The registry's refusal.
        #[source]
        source: cla::Error,
    },

    /// A configured service or application could not be registered: its
    /// service id is taken, is the administrative endpoint, or names a
    /// scheme with no node id configured.
    #[error("Failed to register service {service_id}: {source}")]
    Service {
        /// The service id it was configured under.
        service_id: ServiceId,
        /// The registry's refusal; boxed, as `services::Error` is large.
        #[source]
        source: Box<services::Error>,
    },

    /// A configured routing agent failed to register.
    #[error("Failed to register routing agent: {0}")]
    Routing(#[from] routing::Error),
}

// A configured CLA, its name validated at build().
struct PendingCla {
    name: String,
    factory: BoxClaFactory,
    policy: Option<Arc<dyn FlowControllerFactory>>,
}

// A configured service or application, its service id validated at build().
struct PendingService {
    service_id: ServiceId,
    factory: BoxServiceFactory,
}

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
    services: Vec<PendingService>,
    clas: Vec<PendingCla>,
    rib_builder: RibBuilder,
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
            rib_builder: RibBuilder::new(),
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
        self.rib_builder.service_priority(priority);
        self
    }

    /// Configures a CLA, built and registered under `name` when the BPA
    /// starts.
    ///
    /// `factory` builds the CLA: a CLA crate's own factory, such as a built
    /// `Tcpclv4`, or any built CLA paired with its declarations as a
    /// [`Registration`](cla::Registration). At [`Bpa::start`] the BPA reads
    /// the factory's declarations, folds their `max_bundle_size` with its
    /// own configured cap, and builds the CLA against those
    /// [`Terms`](cla::Terms); [`Cla::on_register`](cla::Cla::on_register)
    /// then receives the same cap. A duplicate name is returned by
    /// [`build()`](Self::build) as [`Error::Cla`].
    pub fn cla<F: Factory + 'static>(
        mut self,
        name: impl Into<String>,
        factory: F,
        policy: Option<Arc<dyn FlowControllerFactory>>,
    ) -> Self {
        self.clas.push(PendingCla {
            name: name.into(),
            factory: BoxClaFactory::new(factory),
            policy,
        });
        self
    }

    /// Configures a low-level service, built and registered as
    /// `service_id` when the BPA starts.
    ///
    /// `factory` builds the service: a service crate's own factory, or an
    /// already built `Arc<S>`. At [`Bpa::start`] the BPA resolves the
    /// endpoint and builds the service there with its sink in the
    /// [`ServiceContext`](services::ServiceContext). A refused service id is
    /// returned by [`build()`](Self::build) as [`Error::Service`].
    pub fn service<F: ServiceFactory + 'static>(
        mut self,
        service_id: ServiceId,
        factory: F,
    ) -> Self {
        self.services.push(PendingService {
            service_id,
            factory: BoxServiceFactory::service(factory),
        });
        self
    }

    /// Configures a high-level application, built and registered as
    /// `service_id` when the BPA starts, as [`service()`](Self::service)
    /// does for a service.
    pub fn application<F: ApplicationFactory + 'static>(
        mut self,
        service_id: ServiceId,
        factory: F,
    ) -> Self {
        self.services.push(PendingService {
            service_id,
            factory: BoxServiceFactory::application(factory),
        });
        self
    }

    /// Configures a routing agent, built and registered under `name` when
    /// the BPA is built.
    ///
    /// `factory` builds the agent: an agent crate's own factory, or an
    /// already built `Arc<A>`, handed its sink and the node ids in the
    /// [`AgentContext`](routing::AgentContext). A duplicate name, or a
    /// factory refusing to build, is returned by [`build()`](Self::build)
    /// as [`Error::Routing`].
    pub fn routing_agent<F: AgentFactory + 'static>(
        mut self,
        name: impl Into<String>,
        factory: F,
    ) -> Self {
        self.rib_builder.insert(name.into(), factory);
        self
    }

    /// Set the key provider for BPSec operations.
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

    /// Consumes the builder and constructs the BPA with every configured
    /// component.
    ///
    /// The configured CLAs, services and applications are validated here,
    /// in configuration order, against each other and the node ids; the
    /// CLAs are built, and every component goes live, at [`Bpa::start`].
    ///
    /// # Errors
    ///
    /// The first configured component that cannot be registered, as the
    /// matching [`Error`](enum@Error) variant.
    pub async fn build(self) -> Result<Bpa> {
        // Freeze the filter packs first: the per-hook chains splice in call
        // order, and each input hook's P = the max peek its registrations
        // declare.
        let filter_chains = FilterChains::freeze(self.filter_packs);

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

        // Both registries park their configured registrations: activation
        // happens in Bpa::start, after storage recovery.
        let service_registry = build_services(self.services, &node_ids)?.build();
        let cla_registry =
            build_clas(self.clas)?.build(&node_ids, self.poll_channel_depth.into(), &rib, &store);

        // TODO: Dispatcher and ClaRegistry still reference each other: the
        // registry holds the dispatcher, and the dispatcher late-binds the
        // registry through its Once cell — readers driven from Rib::start
        // (poll_waiting) rest on a runtime expect rather than the type
        // system. The start closure fixes the wiring order; the cycle
        // itself remains to be dissolved.
        start_dispatcher(cla_registry.clone());

        Ok(Bpa::from_parts(
            node_ids,
            store,
            rib,
            cla_registry,
            service_registry,
            dispatcher,
        ))
    }
}

// Parks the configured CLAs, in configuration order, in the registry
// builder, which refuses a duplicate name.
fn build_clas(clas: Vec<PendingCla>) -> Result<ClaRegistryBuilder> {
    let mut builder = ClaRegistryBuilder::new();
    for PendingCla {
        name,
        factory,
        policy,
    } in clas
    {
        builder
            .insert(name.clone(), factory, policy)
            .map_err(|source| Error::Cla { name, source })?;
    }
    Ok(builder)
}

// Parks the configured services and applications, in configuration order,
// in the registry builder, which refuses a taken, administrative, or
// unresolvable service id.
fn build_services(
    services: Vec<PendingService>,
    node_ids: &NodeIds,
) -> Result<ServiceRegistryBuilder> {
    let mut builder = ServiceRegistryBuilder::new();
    for PendingService {
        service_id,
        factory,
    } in services
    {
        builder
            .insert(service_id.clone(), factory, node_ids)
            .map_err(|source| Error::Service {
                service_id,
                source: Box::new(source),
            })?;
    }
    Ok(builder)
}
