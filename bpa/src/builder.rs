use alloc::{boxed::Box, string::String, vec::Vec};
use core::{
    fmt::{self, Display, Formatter},
    num::{NonZeroU64, NonZeroUsize},
};

use hardy_bpv7::eid::Service as ServiceId;
use hardy_eid_patterns::EidPattern;
use thiserror::Error;

// Aliased: this module's own `Error` is the build error; the crate-root
// generic error is what a deferred conversion raises.
use crate::{
    Arc, Error as ConversionError,
    bpa::Bpa,
    cla::{self, Registration, registry::ClaRegistryBuilder},
    dispatcher::{Config as DispatcherConfig, Dispatcher},
    filter::pack::{FilterPack, chains::FilterChains},
    keys::KeyProvider,
    node_ids::NodeIds,
    policy::FlowControllerFactory,
    routing::{self, RibBuilder, RoutingAgent},
    services::{
        self, Application, Service,
        registry::{ServiceImpl, ServiceRegistryBuilder},
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
    /// A configured CLA failed to convert into a [`Registration`].
    #[error("Failed to create CLA '{name}': {source}")]
    ClaConversion {
        /// The name the CLA was configured under.
        name: String,
        /// The conversion's own error.
        #[source]
        source: ConversionError,
    },

    /// A configured CLA could not be registered: its name is taken.
    #[error("Failed to register CLA '{name}': {source}")]
    Cla {
        /// The name the CLA was configured under.
        name: String,
        /// The registry's refusal.
        #[source]
        source: cla::Error,
    },

    /// A configured service or application failed to convert.
    #[error("Failed to create service {}: {source}", ServiceIds(service_ids))]
    ServiceConversion {
        /// The service ids it was configured under.
        service_ids: Vec<ServiceId>,
        /// The conversion's own error.
        #[source]
        source: ConversionError,
    },

    /// A configured service or application could not be registered on its
    /// service ids: one is taken or repeated, is the administrative
    /// endpoint, or names a scheme with no node id configured. None of the
    /// ids is registered.
    #[error("Failed to register service {}: {source}", ServiceIds(service_ids))]
    Service {
        /// The service ids it was configured under.
        service_ids: Vec<ServiceId>,
        /// The registry's refusal; boxed, as `services::Error` is large.
        #[source]
        source: Box<services::Error>,
    },

    /// A configured routing agent failed to register.
    #[error("Failed to register routing agent: {0}")]
    Routing(#[from] routing::Error),
}

// A configured CLA, converted at build().
struct PendingCla {
    name: String,
    policy: Option<Arc<dyn FlowControllerFactory>>,
    convert: Box<dyn FnOnce() -> core::result::Result<Registration, ConversionError> + Send>,
}

// A configured service or application, converted at build().
struct PendingService {
    service_ids: Vec<ServiceId>,
    convert: Box<dyn FnOnce() -> core::result::Result<ServiceImpl, ConversionError> + Send>,
}

// Writes a configured id set as the ids, comma separated.
struct ServiceIds<'a>(&'a [ServiceId]);

impl Display for ServiceIds<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        for (i, service_id) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{service_id}")?;
        }
        Ok(())
    }
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

    /// Configures a CLA, registered under `name` when the BPA starts.
    ///
    /// `cla` is anything that converts into a [`Registration`]: a CLA
    /// crate's configuration type, or an already-built CLA paired with its
    /// declarations by [`Registration::new`]. The conversion's error may be
    /// any type that boxes into the crate's generic
    /// [`Error`](type@crate::Error). The conversion runs in
    /// [`build()`](Self::build), which returns its error, or a duplicate
    /// name, as [`Error`](enum@Error). The declarations' `max_bundle_size`
    /// is the CLA's receive limit, folded with the BPA's configured cap into
    /// the effective value delivered to
    /// [`Cla::on_register`](crate::cla::Cla::on_register).
    pub fn cla<C>(
        mut self,
        name: impl Into<String>,
        cla: C,
        policy: Option<Arc<dyn FlowControllerFactory>>,
    ) -> Self
    where
        C: TryInto<Registration> + Send + 'static,
        C::Error: Into<ConversionError>,
    {
        self.clas.push(PendingCla {
            name: name.into(),
            policy,
            convert: Box::new(move || cla.try_into().map_err(Into::into)),
        });
        self
    }

    /// Configures a low-level service, registered as `service_id` when the
    /// BPA starts.
    ///
    /// `service` is anything that converts into an `Arc<dyn Service>`:
    /// an already-built `Arc<S>`, or a service crate's configuration type.
    /// The conversion runs in [`build()`](Self::build), which returns its
    /// error, or a refused service id, as [`Error`](enum@Error).
    pub fn service<S>(self, service_id: ServiceId, service: S) -> Self
    where
        S: TryInto<Arc<dyn Service>> + Send + 'static,
        S::Error: Into<ConversionError>,
    {
        self.service_endpoints([service_id], service)
    }

    /// Configures one low-level service on several endpoints, registered
    /// when the BPA starts, all or none.
    ///
    /// Each id becomes its own registration with its own sink, as
    /// [`BpaRegistration::register_service_endpoints`](crate::bpa::BpaRegistration::register_service_endpoints)
    /// describes. [`build()`](Self::build) refuses the whole set if any id
    /// is taken (by this or another configured registration), repeated,
    /// the administrative endpoint, or has no node id for its scheme.
    pub fn service_endpoints<S>(
        mut self,
        service_ids: impl IntoIterator<Item = ServiceId>,
        service: S,
    ) -> Self
    where
        S: TryInto<Arc<dyn Service>> + Send + 'static,
        S::Error: Into<ConversionError>,
    {
        self.services.push(PendingService {
            service_ids: service_ids.into_iter().collect(),
            convert: Box::new(move || {
                service
                    .try_into()
                    .map(ServiceImpl::LowLevel)
                    .map_err(Into::into)
            }),
        });
        self
    }

    /// Configures a high-level application, registered as `service_id`
    /// when the BPA starts.
    ///
    /// `application` is anything that converts into an
    /// `Arc<dyn Application>`, converted in [`build()`](Self::build) as for
    /// [`service()`](Self::service).
    pub fn application<A>(self, service_id: ServiceId, application: A) -> Self
    where
        A: TryInto<Arc<dyn Application>> + Send + 'static,
        A::Error: Into<ConversionError>,
    {
        self.application_endpoints([service_id], application)
    }

    /// Configures one high-level application on several endpoints, all or
    /// none, as [`service_endpoints()`](Self::service_endpoints) does for a
    /// service.
    pub fn application_endpoints<A>(
        mut self,
        service_ids: impl IntoIterator<Item = ServiceId>,
        application: A,
    ) -> Self
    where
        A: TryInto<Arc<dyn Application>> + Send + 'static,
        A::Error: Into<ConversionError>,
    {
        self.services.push(PendingService {
            service_ids: service_ids.into_iter().collect(),
            convert: Box::new(move || {
                application
                    .try_into()
                    .map(ServiceImpl::Application)
                    .map_err(Into::into)
            }),
        });
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
    /// The configured CLAs, services and applications are converted here,
    /// in configuration order, and validated against each other and the
    /// node ids; they go live at [`Bpa::start`].
    ///
    /// # Errors
    ///
    /// The first configured component that fails to convert or register,
    /// as the matching [`Error`](enum@Error) variant.
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

// Converts the configured CLAs, in configuration order, into the registry
// builder, which refuses a duplicate name.
fn build_clas(clas: Vec<PendingCla>) -> Result<ClaRegistryBuilder> {
    let mut builder = ClaRegistryBuilder::new();
    for PendingCla {
        name,
        policy,
        convert,
    } in clas
    {
        let Registration { cla, init } = convert().map_err(|source| Error::ClaConversion {
            name: name.clone(),
            source,
        })?;
        builder
            .insert(name.clone(), cla, policy, init)
            .map_err(|source| Error::Cla { name, source })?;
    }
    Ok(builder)
}

// Converts the configured services and applications, in configuration
// order, into the registry builder, which refuses a taken, administrative,
// or unresolvable service id.
fn build_services(
    services: Vec<PendingService>,
    node_ids: &NodeIds,
) -> Result<ServiceRegistryBuilder> {
    let mut builder = ServiceRegistryBuilder::new();
    for PendingService {
        service_ids,
        convert,
    } in services
    {
        let service = convert().map_err(|source| Error::ServiceConversion {
            service_ids: service_ids.clone(),
            source,
        })?;
        builder
            .insert(service_ids.clone(), service, node_ids)
            .map_err(|source| Error::Service {
                service_ids,
                source: Box::new(source),
            })?;
    }
    Ok(builder)
}
