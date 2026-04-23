use hardy_async::async_trait;
use hardy_bpv7::eid::NodeId;
#[cfg(feature = "instrument")]
use tracing::instrument;

use crate::builder::BpaBuilder;
use crate::cla::registry::ClaRegistry;
use crate::cla::{self, Cla};
use crate::dispatcher::Dispatcher;
use crate::filters::registry::Registry as FilterRegistry;
use crate::filters::{self, Filter, Hook};
use crate::policy::EgressPolicy;
use crate::rib::Rib;
use crate::routes::{self, RoutingAgent};
use crate::services::Service;
use crate::services::registry::ServiceRegistry;
use crate::storage::Store;
use crate::{Arc, otel_metrics, services};

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
/// in [`cla::Cla::on_register`]. Key Sink methods:
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
/// [`services::ApplicationSink`] (high-level, payload-only), provided in their
/// respective `on_register` methods.
#[async_trait]
pub trait BpaRegistration: Send + Sync {
    /// Register a Convergence Layer Adapter with the BPA.
    ///
    /// The CLA will receive a [`cla::Sink`] via [`cla::Cla::on_register`]
    /// for communicating back to the BPA.
    ///
    /// # Arguments
    ///
    /// * `name` - Unique name for this CLA instance
    /// * `cla` - The CLA implementation
    /// * `policy` - Optional egress policy for traffic shaping
    ///
    /// # Returns
    ///
    /// The BPA's node IDs on success
    async fn register_cla(
        &self,
        name: String,
        cla: Arc<dyn Cla>,
        policy: Option<Arc<dyn EgressPolicy>>,
    ) -> cla::Result<Vec<hardy_bpv7::eid::NodeId>>;

    /// Register a low-level Service with full bundle access.
    async fn register_service(
        &self,
        service_id: hardy_bpv7::eid::Service,
        service: Arc<dyn Service>,
    ) -> services::Result<hardy_bpv7::eid::Eid>;

    /// Register a high-level Application with payload-only access.
    async fn register_application(
        &self,
        service_id: hardy_bpv7::eid::Service,
        application: Arc<dyn services::Application>,
    ) -> services::Result<hardy_bpv7::eid::Eid>;

    /// Register a low-level Service with a dynamically assigned service ID.
    async fn register_dynamic_service(
        &self,
        service: Arc<dyn Service>,
    ) -> services::Result<hardy_bpv7::eid::Eid>;

    /// Register a high-level Application with a dynamically assigned service ID.
    async fn register_dynamic_application(
        &self,
        application: Arc<dyn services::Application>,
    ) -> services::Result<hardy_bpv7::eid::Eid>;

    /// Register a Routing Agent with the BPA.
    ///
    /// The routing agent will receive a [`routes::RoutingSink`] via
    /// [`routes::RoutingAgent::on_register`] for managing routes in the RIB.
    ///
    /// # Arguments
    ///
    /// * `name` - Unique name for this routing agent instance (used as route source)
    /// * `agent` - The routing agent implementation
    ///
    /// # Returns
    ///
    /// The BPA's node IDs on success
    async fn register_routing_agent(
        &self,
        name: String,
        agent: Arc<dyn RoutingAgent>,
    ) -> routes::Result<Vec<hardy_bpv7::eid::NodeId>>;
}

/// The core Bundle Processing Agent (RFC 9171).
///
/// Holds references to the store, RIB, CLA/service/filter registries, and
/// dispatcher. Construct via [`BpaBuilder`] (obtained from [`Bpa::builder()`]).
///
/// After construction, call [`start()`](Bpa::start) to begin processing and
/// [`shutdown()`](Bpa::shutdown) for ordered teardown.
pub struct Bpa {
    node_ids: Arc<crate::node_ids::NodeIds>,
    store: Arc<Store>,
    rib: Arc<Rib>,
    cla_registry: Arc<ClaRegistry>,
    service_registry: Arc<ServiceRegistry>,
    filter_registry: Arc<FilterRegistry>,
    dispatcher: Arc<Dispatcher>,
}

impl Bpa {
    pub(crate) fn from_parts(
        node_ids: Arc<crate::node_ids::NodeIds>,
        store: Arc<Store>,
        rib: Arc<Rib>,
        cla_registry: Arc<ClaRegistry>,
        service_registry: Arc<ServiceRegistry>,
        filter_registry: Arc<FilterRegistry>,
        dispatcher: Arc<Dispatcher>,
    ) -> Self {
        Self {
            node_ids,
            store,
            rib,
            cla_registry,
            service_registry,
            filter_registry,
            dispatcher,
        }
    }

    pub fn builder() -> BpaBuilder {
        BpaBuilder::new()
    }

    /// Reconcile storage after an unclean shutdown.
    ///
    /// Spawns the three-phase recovery protocol as a background task.
    /// Call before [`start()`](Bpa::start).
    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    pub fn recover(&self) {
        let store = self.store.clone();
        let dispatcher = self.dispatcher.clone();
        hardy_async::spawn!(self.store.tasks(), "recovery", async move {
            crate::recover::Recovery::new(&store, &dispatcher)
                .mark()
                .await
                .reconcile()
                .await
                .purge()
                .await;
        });
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    pub fn start(&self) {
        otel_metrics::init();

        // Start the store
        self.store.start(self.dispatcher.clone());

        // Start the RIB
        self.rib.start(self.dispatcher.clone());
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    pub async fn shutdown(&self) {
        // Shutdown order is critical for clean termination:
        //
        // 1. Routing agents - Remove dynamic routes (prevents new forwarding decisions)
        // 2. CLAs - Stop external bundle sources (network I/O)
        // 3. Services - Stop internal bundle sources (applications calling sink.send())
        // 4. Dispatcher - Drain remaining in-flight bundles (all sources now closed)
        // 5. RIB - No more routing lookups needed
        // 6. Store - No more data access needed
        //
        // Routing agents shut down first so their routes are removed before CLAs
        // drain. CLAs and Services must shut down BEFORE dispatcher because they
        // are bundle sources. The dispatcher's processing pool may have tasks
        // blocked on CLA forwarding or waiting for service responses.

        self.rib.shutdown_agents().await;
        self.cla_registry.shutdown().await;
        self.service_registry
            .shutdown(&self.node_ids, &self.rib)
            .await;
        self.dispatcher.shutdown().await;
        self.rib.shutdown().await;
        self.store.shutdown().await;
        self.filter_registry.clear();
    }

    /// Register a filter at a hook point
    #[cfg_attr(feature = "instrument", instrument(skip(self, filter)))]
    pub fn register_filter(
        &self,
        hook: Hook,
        name: &str,
        after: &[&str],
        filter: Filter,
    ) -> Result<(), filters::Error> {
        self.filter_registry.register(hook, name, after, filter)
    }

    /// Unregister a filter by name from a hook point
    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    pub fn unregister_filter(
        &self,
        hook: Hook,
        name: &str,
    ) -> Result<Option<Filter>, filters::Error> {
        self.filter_registry.unregister(hook, name)
    }
}

#[async_trait]
impl BpaRegistration for Bpa {
    #[cfg_attr(feature = "instrument", instrument(skip(self, application)))]
    async fn register_application(
        &self,
        service_id: hardy_bpv7::eid::Service,
        application: Arc<dyn services::Application>,
    ) -> services::Result<hardy_bpv7::eid::Eid> {
        self.service_registry
            .register_application(
                service_id,
                application,
                &self.node_ids,
                &self.rib,
                &self.dispatcher,
            )
            .await
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, service)))]
    async fn register_service(
        &self,
        service_id: hardy_bpv7::eid::Service,
        service: Arc<dyn services::Service>,
    ) -> services::Result<hardy_bpv7::eid::Eid> {
        self.service_registry
            .register_service(
                service_id,
                service,
                &self.node_ids,
                &self.rib,
                &self.dispatcher,
            )
            .await
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, cla, policy)))]
    async fn register_cla(
        &self,
        name: String,
        cla: Arc<dyn Cla>,
        policy: Option<Arc<dyn EgressPolicy>>,
    ) -> cla::Result<Vec<NodeId>> {
        self.cla_registry
            .register(name, cla, &self.dispatcher, policy)
            .await
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, service)))]
    async fn register_dynamic_service(
        &self,
        service: Arc<dyn services::Service>,
    ) -> services::Result<hardy_bpv7::eid::Eid> {
        self.service_registry
            .register_dynamic_service(service, &self.node_ids, &self.rib, &self.dispatcher)
            .await
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, application)))]
    async fn register_dynamic_application(
        &self,
        application: Arc<dyn services::Application>,
    ) -> services::Result<hardy_bpv7::eid::Eid> {
        self.service_registry
            .register_dynamic_application(application, &self.node_ids, &self.rib, &self.dispatcher)
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
