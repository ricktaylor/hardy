//! Bundle Protocol Agent (RFC 9171).
//!
//! The BPA is the central processing entity. CLAs and AAs communicate
//! through it; it delegates to the Ingress/Egress pipelines internally.

use alloc::sync::{Arc, Weak};

use BundleId;
use bytes::Bytes;
use hardy_async::BoundedTaskPool;
use hardy_async::TaskPool;
use hardy_async::async_trait;
use hardy_async::sync::spin::Once;
use hardy_bpv7::eid::{Eid, NodeId, Service as EidService};
use hardy_bpv7::status_report::ReasonCode;
use trace_err::*;
use tracing::warn;

#[cfg(feature = "instrument")]
use tracing::instrument;

use crate::builder::BpaBuilder;
use crate::bundle::{Bundle, BundleStatus};
use crate::cla::policy::EgressPolicy;
use crate::cla::registry::ClaRegistry;
use crate::cla::{self, Cla, ClaAddress};
use crate::egress::Egress;
use crate::filter::{self, Filter, FilterEngine, Hook};
use crate::ingress::{Ingress, IngressResult};
use crate::metrics::reason_label;
use crate::node_ids::NodeIds;
use crate::rib::{FindResult, Rib};
use crate::routes::{self, RoutingAgent};
use crate::security::KeyStore;
use crate::security::pattern::PatternKeySource;
use crate::services::registry::{DeliverySink, ServiceRegistry};
use crate::services::{self, Application, Service};
use crate::sink::Sink;
use crate::storage::{self, Store};

mod admin;
mod dispatch;
mod forward;
mod local;
mod reassemble;
mod report;

/// Trait for registering CLAs, services, and applications with a BPA.
///
/// This trait abstracts the registration interface, allowing components
/// to work with either a local [`Bpa`] instance or a remote BPA via gRPC.
///
/// # Component Lifecycle
///
/// Components follow a consistent lifecycle pattern:
///
/// 1. **Construction**: `new(&Config) -> Result<Self, Error>` validates configuration
///    eagerly. Errors surface at construction time rather than during registration.
///
/// 2. **Registration**: `register(&Arc<Self>, &dyn BpaRegistration)` calls the
///    appropriate `register_*` method. The BPA calls `on_register()` on the component,
///    providing a Sink for communication back to the BPA.
///
/// 3. **Active**: Component uses Sink methods to interact with the BPA. The Sink
///    remains valid until unregistration.
///
/// 4. **Unregistration**: Either the component calls `sink.unregister()`, or the BPA
///    initiates shutdown and calls `on_unregister()`.
///
/// # Sink Storage Requirement
///
/// **Components MUST store the Sink for their entire active lifetime.**
///
/// The Sink is provided in `on_register()` and must be retained (typically in
/// a `spin::Once<T>` or `OnceLock<T>`) until unregistration. If `on_register()`
/// returns without storing the Sink, the Sink is dropped and the component is
/// automatically unregistered.
///
/// ```ignore
/// pub struct MyComponent {
///     sink: spin::Once<Box<dyn Sink>>,
///     // ... other fields
/// }
///
/// impl MyTrait for MyComponent {
///     fn on_register(&self, sink: Box<dyn Sink>) {
///         // MUST store the sink - dropping it triggers unregistration
///         self.sink.set(sink);
///     }
/// }
/// ```
///
/// # Post-Disconnection Behaviour
///
/// After unregistration, the Sink remains stored but becomes non-functional:
/// all operations return `Error::Disconnected`. Components don't need defensive
/// patterns like `Option<Sink>` with `take()` in `on_unregister()` - the Sink
/// can remain stored and post-disconnection calls simply fail gracefully.
///
/// This means `on_unregister()` only handles component-specific cleanup (stopping
/// tasks, closing connections), not Sink lifecycle management.
///
/// # Recommended Implementation Pattern
///
/// ```ignore
/// impl MyComponent {
///     /// Creates a new component. Validates configuration eagerly.
///     pub fn new(config: &Config) -> Result<Self, Error> {
///         // Validate and prepare resources
///         Ok(Self { sink: spin::Once::new(), /* ... */ })
///     }
///
///     /// Registers with the BPA. Returns after Sink is stored.
///     pub async fn register(
///         self: &Arc<Self>,
///         bpa: &dyn BpaRegistration,
///     ) -> Result<(), Error> {
///         bpa.register_xxx(/* ... */, self.clone(), /* ... */).await?;
///         Ok(())
///     }
///
///     /// Explicit unregistration.
///     pub async fn unregister(&self) {
///         if let Some(sink) = self.sink.get() {
///             sink.unregister().await;
///         }
///     }
/// }
/// ```
///
/// # For CLA Implementors
///
/// CLAs receive callbacks via the [`cla::Sink`] trait, which is provided
/// in [`Cla::on_register`]. Key Sink methods:
///
/// - `dispatch()` - Submit received bundles to the BPA
/// - `add_peer()` / `remove_peer()` - Manage peer connections (keyed by CL address)
/// - `unregister()` - Disconnect from the BPA
///
/// # For Routing Agent Implementors
///
/// Routing agents receive [`routes::RoutingSink`] in
/// [`routes::RoutingAgent::on_register`]. Key Sink methods:
///
/// - `add_route()` / `remove_route()` - Manage routes in the RIB (source auto-injected)
/// - `unregister()` - Disconnect from the BPA
///
/// For simple static route sets, use [`routes::StaticRoutingAgent`] instead
/// of implementing the trait manually.
///
/// # For Service Implementors
///
/// Services receive [`services::ServiceSink`] (low-level, full bundle access) or
/// [`ApplicationSink`] (high-level, payload-only), provided in their
/// respective `on_register` methods.
#[async_trait]
pub trait BpaRegistration: Send + Sync {
    /// Register a Convergence Layer Adapter with the BPA.
    async fn register_cla(
        &self,
        name: String,
        cla: Arc<dyn Cla>,
        policy: Option<Arc<dyn EgressPolicy>>,
    ) -> cla::Result<Vec<NodeId>>;

    /// Register a low-level Service with full bundle access.
    async fn register_service(
        &self,
        service_id: EidService,
        service: Arc<dyn Service>,
    ) -> services::Result<Eid>;

    /// Register a high-level Application with payload-only access.
    async fn register_application(
        &self,
        service_id: EidService,
        application: Arc<dyn Application>,
    ) -> services::Result<Eid>;

    /// Register a low-level Service with a dynamically assigned service ID.
    async fn register_dynamic_service(&self, service: Arc<dyn Service>) -> services::Result<Eid>;

    /// Register a high-level Application with a dynamically assigned service ID.
    async fn register_dynamic_application(
        &self,
        application: Arc<dyn Application>,
    ) -> services::Result<Eid>;

    /// Register a Routing Agent with the BPA.
    async fn register_routing_agent(
        &self,
        name: String,
        agent: Arc<dyn RoutingAgent>,
    ) -> routes::Result<Vec<NodeId>>;
}

/// The core Bundle Processing Agent (RFC 9171).
///
/// Central processing entity through which all CLAs and AAs communicate.
/// Internally delegates to standalone pipeline modules (Ingress, Egress).
///
/// Construct via [`BpaBuilder`] (obtained from [`Bpa::builder()`]).
/// After construction, call [`start()`](Bpa::start) to begin processing and
/// [`shutdown()`](Bpa::shutdown) for ordered teardown.
pub struct Bpa {
    // Identity
    node_ids: Arc<NodeIds>,

    // Processing
    tasks: TaskPool,
    processing_pool: Arc<BoundedTaskPool>,

    // Pipelines
    pub(crate) ingress: Ingress,
    pub(crate) egress: Egress,

    // Core subsystems
    store: Arc<Store>,
    rib: Arc<Rib>,
    pub(crate) key_store: Arc<KeyStore>,
    filter_engine: Arc<FilterEngine>,

    // Registries (set after construction via Once)
    cla_registry: Once<Arc<ClaRegistry>>,
    service_registry: Once<Arc<ServiceRegistry>>,

    // Self-reference for Arc<Self> recovery from &self
    self_ref: Weak<Self>,

    // Dispatch queue
    pub(crate) dispatch_tx: storage::channel::Sender,

    // Config
    status_reports: bool,
    poll_channel_depth: usize,
}

impl Bpa {
    pub fn builder() -> BpaBuilder {
        BpaBuilder::new()
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_inner(
        status_reports: bool,
        poll_channel_depth: core::num::NonZeroUsize,
        processing_pool_size: core::num::NonZeroUsize,
        node_ids: Arc<NodeIds>,
        store: Arc<Store>,
        rib: Arc<Rib>,
        key_store: Arc<KeyStore>,
        filter_engine: Arc<FilterEngine>,
    ) -> (Arc<Self>, impl FnOnce(&Arc<Self>)) {
        if status_reports {
            warn!("Bundle status reports are enabled");
        }

        let poll_channel_depth_usize: usize = poll_channel_depth.into();

        let (dispatch_tx, dispatch_rx) =
            store.channel(BundleStatus::Dispatching, poll_channel_depth_usize);

        let processing_pool = Arc::new(BoundedTaskPool::new(processing_pool_size));

        let ingress = Ingress {
            store: store.clone(),
            key_store: key_store.clone(),
            filter_engine: filter_engine.clone(),
            processing_pool: processing_pool.clone(),
            rib: rib.clone(),
        };

        let egress = Egress {
            store: store.clone(),
            key_store: key_store.clone(),
            filter_engine: filter_engine.clone(),
            processing_pool: processing_pool.clone(),
        };

        let bpa = Arc::new_cyclic(|weak| Self {
            tasks: TaskPool::new(),
            processing_pool,
            ingress,
            egress,
            store,
            rib,
            key_store,
            filter_engine,
            cla_registry: Once::new(),
            service_registry: Once::new(),
            self_ref: weak.clone(),
            dispatch_tx,
            status_reports,
            node_ids,
            poll_channel_depth: poll_channel_depth_usize,
        });

        (bpa, |b: &Arc<Self>| {
            let bpa = b.clone();
            hardy_async::spawn!(b.tasks, "dispatch_queue_consumer", async move {
                bpa.run_dispatch_queue(dispatch_rx).await
            });
        })
    }

    pub(crate) fn set_cla_registry(&self, registry: Arc<ClaRegistry>) {
        self.cla_registry.call_once(|| registry);
    }

    pub(crate) fn set_service_registry(&self, registry: Arc<ServiceRegistry>) {
        self.service_registry.call_once(|| registry);
    }

    /// Get a strong reference to this Bpa from &self.
    fn arc(&self) -> Arc<Self> {
        self.self_ref.upgrade().trace_expect("Bpa has been dropped")
    }

    fn cla_registry(&self) -> &Arc<ClaRegistry> {
        self.cla_registry
            .get()
            .trace_expect("CLA registry not initialized")
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    pub fn start(self: &Arc<Self>, recover_storage: bool) {
        crate::metrics::init();
        self.store.start(self.clone(), recover_storage);
        self.rib.start(self.clone());
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    pub async fn shutdown(&self) {
        // Shutdown order is critical for clean termination:
        //
        // 1. Routing agents - Remove dynamic routes (prevents new forwarding decisions)
        // 2. CLAs - Stop external bundle sources (network I/O)
        // 3. Services - Stop internal bundle sources (applications calling sink.send())
        // 4. Processing - Drain remaining in-flight bundles (all sources now closed)
        // 5. RIB - No more routing lookups needed
        // 6. Store - No more data access needed
        //
        // Routing agents shut down first so their routes are removed before CLAs
        // drain. CLAs and Services must shut down BEFORE processing pool because they
        // are bundle sources. The processing pool may have tasks blocked on CLA
        // forwarding or waiting for service responses.

        self.rib.shutdown_agents().await;
        self.cla_registry().shutdown().await;
        self.service_registry()
            .shutdown(&self.node_ids, &self.rib)
            .await;
        self.dispatch_tx.close().await;
        self.processing_pool.shutdown().await;
        self.tasks.shutdown().await;
        self.rib.shutdown().await;
        self.store.shutdown().await;
        self.filter_engine.clear();
    }

    fn service_registry(&self) -> &Arc<ServiceRegistry> {
        self.service_registry
            .get()
            .trace_expect("Service registry not initialized")
    }

    /// Replace the key source used for BPSec operations.
    pub fn set_key_source(&self, source: Arc<PatternKeySource>) {
        self.key_store.set(source);
    }

    /// Register a filter at a hook point
    #[cfg_attr(feature = "instrument", instrument(skip(self, filter)))]
    pub fn register_filter(
        &self,
        hook: Hook,
        name: &str,
        after: &[&str],
        filter: Filter,
    ) -> Result<(), filter::Error> {
        self.filter_engine.register(hook, name, after, filter)
    }

    /// Unregister a filter by name from a hook point
    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    pub fn unregister_filter(
        &self,
        hook: Hook,
        name: &str,
    ) -> Result<Option<Filter>, filter::Error> {
        self.filter_engine.unregister(hook, name)
    }

    // -- Bundle pipeline: ingress → dispatch → egress --

    /// CLA → BPA: decode + Hook::Ingress.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn receive(
        &self,
        data: Bytes,
        ingress_cla: Option<Arc<str>>,
        ingress_peer_node: Option<NodeId>,
        ingress_peer_addr: Option<ClaAddress>,
    ) -> Result<(), crate::Error> {
        // 1. Ingress
        let (bundle, route) = match self
            .ingress
            .receive(data, ingress_cla, ingress_peer_node, ingress_peer_addr)
            .await?
        {
            IngressResult::Routed(bundle, route) if route != FindResult::Wait => (bundle, route),
            _ => {
                todo!()
            }
        };

        // 2. Dispatch
        let sink = self.dispatch(&route);

        // 3. Egress
        self.egress.send(bundle, sink.as_ref()).await.ok();
        Ok(())
    }

    /// Service → BPA: build bundle from payload, Hook::Originate.
    #[cfg_attr(feature = "instrument", instrument(skip(self, payload)))]
    pub async fn originate(
        &self,
        source: Eid,
        destination: Eid,
        payload: Bytes,
        lifetime: core::time::Duration,
        flags: hardy_bpv7::bundle::Flags,
    ) -> Result<BundleId, services::Error> {
        // Build
        let mut builder = hardy_bpv7::builder::Builder::new(source, destination.clone())
            .with_lifetime(lifetime)
            .with_flags(flags);

        if flags.receipt_report_requested
            || flags.forward_report_requested
            || flags.delivery_report_requested
            || flags.delete_report_requested
        {
            builder = builder.with_report_to(self.node_ids.get_admin_endpoint(&destination));
        }

        let (bundle, data) = builder
            .with_payload(alloc::borrow::Cow::Borrowed(&payload))
            .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
            .map_err(|e| services::Error::Internal(e.into()))?;

        let bundle_id = bundle.id.clone();
        let bundle = Bundle {
            metadata: crate::bundle::BundleMetadata {
                status: BundleStatus::Dispatching,
                ..Default::default()
            },
            bundle,
        };
        let data = Bytes::from(data);

        // 1. Ingress
        let (bundle, route) = match self
            .ingress
            .process(bundle, data, Some(filter::Hook::Originate))
            .await
            .map_err(services::Error::Internal)?
        {
            IngressResult::Routed(bundle, route) if route != FindResult::Wait => (bundle, route),
            IngressResult::Routed(_, _) => {
                todo!("handle Wait for originated bundles")
            }
            IngressResult::Duplicate => return Err(services::Error::DuplicateBundle),
            IngressResult::Dropped => return Err(services::Error::Dropped(None)),
        };

        ::metrics::counter!("bpa.bundle.originated").increment(1);

        // 2. Dispatch
        let sink = self.dispatch(&route);

        // 3. Egress
        self.egress.send(bundle, sink.as_ref()).await.ok();
        Ok(bundle_id)
    }

    /// Internal status report: build admin record bundle, no filter hook.
    async fn report(&self, payload: Vec<u8>, report_to: &Eid) {
        if !self.status_reports {
            return;
        }

        // Build
        let Ok((bundle, data)) = hardy_bpv7::builder::Builder::new(
            self.node_ids.get_admin_endpoint(report_to),
            report_to.clone(),
        )
        .with_flags(hardy_bpv7::bundle::Flags {
            is_admin_record: true,
            ..Default::default()
        })
        .with_payload(payload.into())
        .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
        else {
            tracing::error!("Failed to build status report bundle");
            return;
        };

        let bundle = Bundle {
            metadata: crate::bundle::BundleMetadata {
                status: BundleStatus::Dispatching,
                ..Default::default()
            },
            bundle,
        };
        let data = Bytes::from(data);

        // 1. Ingress
        let (bundle, route) = match self.ingress.process(bundle, data, None).await {
            Ok(IngressResult::Routed(bundle, route)) if route != FindResult::Wait => {
                (bundle, route)
            }
            Ok(IngressResult::Routed(_, _)) => {
                todo!("handle Wait for status reports")
            }
            Ok(IngressResult::Dropped | IngressResult::Duplicate) => {
                tracing::debug!("Status report dropped");
                return;
            }
            Err(e) => {
                tracing::error!("Status report ingress failed: {e}");
                return;
            }
        };

        // 2. Dispatch
        let sink = self.dispatch(&route);

        // 3. Egress
        self.egress.send(bundle, sink.as_ref()).await.ok();
    }

    /// Select the appropriate Sink for a route.
    fn dispatch(&self, route: &FindResult) -> Box<dyn Sink> {
        match route {
            FindResult::Forward(_peer) => {
                todo!("streaming model: return CLA Sink for peer")
            }
            FindResult::Deliver(service) => Box::new(DeliverySink {
                service: service.clone(),
            }),
            FindResult::AdminEndpoint => {
                todo!("admin sink")
            }
            FindResult::Wait => {
                todo!("wait handling before dispatch")
            }
            FindResult::Drop(_) => {
                todo!("drop handling before dispatch")
            }
        }
    }

    // -- Core bundle lifecycle methods --

    /// Load bundle data, dropping the bundle with `DepletedStorage` if the
    /// data is missing and the bundle has not yet expired. Expired-and-missing
    /// bundles are left for the reaper to handle (it will drop them with
    /// `LifetimeExpired`).
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn load_data_or_drop(&self, bundle: Bundle) -> Option<(Bundle, Bytes)> {
        let storage_name = bundle
            .metadata
            .storage_name
            .as_ref()
            .trace_expect("Bundle without storage_name reached load_data_or_drop");

        match self.store.load_data(storage_name).await {
            Some(data) => Some((bundle, data)),
            None => {
                if !bundle.has_expired() {
                    self.drop_bundle(bundle, ReasonCode::DepletedStorage).await;
                }
                None
            }
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, bundle)))]
    pub(crate) async fn drop_bundle(&self, bundle: Bundle, reason: ReasonCode) {
        ::metrics::counter!("bpa.bundle.dropped", "reason" => reason_label(&reason)).increment(1);
        self.report_bundle_deletion(&bundle, reason).await;
        self.delete_bundle(bundle).await
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, bundle)))]
    async fn delete_bundle(&self, bundle: Bundle) {
        if let Some(storage_name) = &bundle.metadata.storage_name {
            self.store.delete_data(storage_name).await;
        }
        self.store.tombstone_metadata(&bundle.bundle.id).await;

        ::metrics::gauge!("bpa.bundle.status", "state" => crate::metrics::status_label(&bundle.metadata.status)).decrement(1.0);
    }

    pub async fn poll_service_waiting(self: &Arc<Self>, source: &Eid) {
        let (tx, rx) = flume::bounded::<Bundle>(self.poll_channel_depth);

        let bpa = self.clone();

        futures::join!(self.store.poll_service_waiting(source.clone(), tx), async {
            while let Ok(mut bundle) = rx.recv_async().await {
                bpa.store
                    .update_status(&mut bundle, &BundleStatus::Dispatching)
                    .await;
                bpa.dispatch_bundle(bundle).await;
            }
        });
    }
}

#[async_trait]
impl BpaRegistration for Bpa {
    #[cfg_attr(feature = "instrument", instrument(skip(self, application)))]
    async fn register_application(
        &self,
        service_id: EidService,
        application: Arc<dyn Application>,
    ) -> services::Result<Eid> {
        self.service_registry()
            .register_application(
                service_id,
                application,
                &self.node_ids,
                &self.rib,
                &self.arc(),
            )
            .await
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, service)))]
    async fn register_service(
        &self,
        service_id: EidService,
        service: Arc<dyn services::Service>,
    ) -> services::Result<Eid> {
        self.service_registry()
            .register_service(service_id, service, &self.node_ids, &self.rib, &self.arc())
            .await
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, cla, policy)))]
    async fn register_cla(
        &self,
        name: String,
        cla: Arc<dyn Cla>,
        policy: Option<Arc<dyn EgressPolicy>>,
    ) -> cla::Result<Vec<NodeId>> {
        self.cla_registry()
            .register(name, cla, &self.arc(), policy)
            .await
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, service)))]
    async fn register_dynamic_service(
        &self,
        service: Arc<dyn services::Service>,
    ) -> services::Result<Eid> {
        self.service_registry()
            .register_dynamic_service(service, &self.node_ids, &self.rib, &self.arc())
            .await
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, application)))]
    async fn register_dynamic_application(
        &self,
        application: Arc<dyn Application>,
    ) -> services::Result<Eid> {
        self.service_registry()
            .register_dynamic_application(application, &self.node_ids, &self.rib, &self.arc())
            .await
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, agent)))]
    async fn register_routing_agent(
        &self,
        name: String,
        agent: Arc<dyn RoutingAgent>,
    ) -> routes::Result<Vec<NodeId>> {
        self.rib.register_agent(name, agent).await
    }
}
