use hardy_async::async_trait;
use hardy_bpv7::eid::NodeId;
#[cfg(feature = "instrument")]
use tracing::instrument;

use crate::{
    Arc,
    builder::BpaBuilder,
    cla::{self, Cla, ClaInit, registry::ClaRegistry},
    dispatcher::Dispatcher,
    otel_metrics,
    policy::FlowControllerFactory,
    routing::{self, Rib, RoutingAgent},
    services::{self, Service, registry::ServiceRegistry},
    storage::store::Store,
};

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
/// Routing agents receive [`routing::RoutingSink`] in
/// [`routing::RoutingAgent::on_register`]. Key Sink methods:
///
/// - `add_route()` / `remove_route()` - Manage routes in the RIB (source auto-injected)
/// - `unregister()` - Disconnect from the BPA
///
/// For simple static route sets, use [`routing::StaticRoutingAgent`] instead
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
    /// Implementations must drive [`cla::Cla::on_register`] to completion
    /// before returning: once this method resolves, callers may rely on
    /// everything `on_register` delivers (the sink, the effective cap)
    /// having reached the CLA.
    ///
    /// # Arguments
    ///
    /// * `name` - Unique name for this CLA instance
    /// * `cla` - The CLA implementation
    /// * `policy` - Optional egress policy for traffic shaping
    /// * `init` - The CLA's set-once declarations ([`cla::ClaInit`]),
    ///   snapshotted at registration. Its `max_bundle_size` is the CLA's
    ///   declared receive limit; the BPA folds it with its own configured
    ///   cap into the effective value delivered to
    ///   [`cla::Cla::on_register`] — dispatch enforces the BPA's
    ///   configured cap, not the folded per-CLA value.
    ///
    /// # Returns
    ///
    /// The BPA's node IDs on success
    async fn register_cla(
        &self,
        name: String,
        cla: Arc<dyn Cla>,
        policy: Option<Arc<dyn FlowControllerFactory>>,
        init: ClaInit,
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
    /// The routing agent will receive a [`routing::RoutingSink`] via
    /// [`routing::RoutingAgent::on_register`] for managing routes in the RIB.
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
    ) -> routing::Result<Vec<hardy_bpv7::eid::NodeId>>;
}

/// The core Bundle Processing Agent (RFC 9171).
///
/// Holds references to the store, RIB, CLA/service registries, and
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
    dispatcher: Arc<Dispatcher>,
}

impl Bpa {
    pub(crate) fn from_parts(
        node_ids: Arc<crate::node_ids::NodeIds>,
        store: Arc<Store>,
        rib: Arc<Rib>,
        cla_registry: Arc<ClaRegistry>,
        service_registry: Arc<ServiceRegistry>,
        dispatcher: Arc<Dispatcher>,
    ) -> Self {
        Self {
            node_ids,
            store,
            rib,
            cla_registry,
            service_registry,
            dispatcher,
        }
    }

    pub fn builder() -> BpaBuilder {
        BpaBuilder::new()
    }

    /// Start the BPA's background machinery.
    ///
    /// When `recover_storage` is set, storage crash recovery runs to
    /// completion before this returns: recovery's checkpoint resets assume
    /// a quiescent store, so CLAs and services must only be registered
    /// after `start` resolves.
    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    pub async fn start(&self, recover_storage: bool) {
        otel_metrics::init();

        // Start the store, awaiting recovery so registrations that follow
        // cannot race the consistency check
        self.store
            .start(self.dispatcher.clone(), recover_storage)
            .await;

        // Only now activate the builder-configured CLAs and services: the
        // quiescent-store precondition above covers the configuration path
        // exactly as it covers dynamic registrations.
        self.cla_registry.start(&self.dispatcher).await;
        self.service_registry
            .start(&self.node_ids, &self.rib, &self.dispatcher)
            .await;

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
        self.service_registry.shutdown(&self.rib).await;
        self.dispatcher.shutdown().await;
        self.rib.shutdown().await;
        self.store.shutdown().await;
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
        policy: Option<Arc<dyn FlowControllerFactory>>,
        init: ClaInit,
    ) -> cla::Result<Vec<NodeId>> {
        self.cla_registry
            .register(name, cla, &self.dispatcher, policy, init)
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
    ) -> routing::Result<Vec<NodeId>> {
        self.rib.register_agent(name, agent).await
    }
}

#[cfg(test)]
mod tests {
    use core::{num::NonZeroU64, time::Duration};

    use hardy_async::sync::spin::Once;
    use hardy_bpv7::{
        bundle::Id,
        // Aliased: the glob import below brings in `services::Service`.
        eid::{Eid, IpnNodeId, Service as ServiceId},
        status_report::ReasonCode,
    };
    use time::OffsetDateTime;
    use tokio::time::timeout;

    use super::*;
    use crate::{
        Bytes,
        stream::{Receiver, Segment},
    };

    /// Fails every `forward` call synchronously, without pulling, recording
    /// the bundle each attempt was for.
    struct FailingCla {
        sink: Once<Box<dyn cla::Sink>>,
        attempts_tx: flume::Sender<Id>,
    }

    #[async_trait]
    impl Cla for FailingCla {
        async fn on_register(
            &self,
            sink: Box<dyn cla::Sink>,
            _node_ids: &[NodeId],
            _max_bundle_size: Option<NonZeroU64>,
        ) {
            self.sink.call_once(|| sink);
        }

        async fn on_unregister(&self) {}

        async fn forward(
            &self,
            _lane: Option<u32>,
            _cla_addr: &cla::ClaAddress,
            bundle_id: &Id,
            _total_len: u64,
            _stream: &mut dyn Receiver<Segment>,
        ) -> cla::Result<cla::ForwardBundleResult> {
            let _ = self.attempts_tx.send(bundle_id.clone());
            Err(cla::Error::StreamCancelled)
        }
    }

    /// Originates bundles; never has any delivered.
    struct SendOnlyApp {
        sink: Once<Box<dyn services::ApplicationSink>>,
    }

    #[async_trait]
    impl services::Application for SendOnlyApp {
        async fn on_register(&self, _source: &Eid, sink: Box<dyn services::ApplicationSink>) {
            self.sink.call_once(|| sink);
        }

        async fn on_unregister(&self) {}

        async fn on_deliver(
            &self,
            _bundle_id: &Id,
            _expiry: OffsetDateTime,
            _ack_requested: bool,
            _total_len: u64,
            _stream: &mut dyn Receiver<Segment>,
        ) -> services::Result<()> {
            Ok(())
        }

        async fn on_status_notify(
            &self,
            _bundle_id: &Id,
            _from: &Eid,
            _kind: services::StatusNotify,
            _reason: ReasonCode,
            _timestamp: Option<OffsetDateTime>,
        ) {
        }
    }

    /// A synchronous per-transfer failure parks only that bundle, with no
    /// inline retry: a deterministic failure must not spin dispatch →
    /// forward → fail, so exactly one attempt occurs until the next routing
    /// or link event.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_forward_does_not_retry_inline() {
        let bpa = Bpa::builder().build().await.unwrap();
        bpa.start(false).await;

        let (attempts_tx, attempts_rx) = flume::unbounded();
        let cla = Arc::new(FailingCla {
            sink: Once::new(),
            attempts_tx,
        });
        bpa.register_cla("failing".to_string(), cla.clone(), None, ClaInit::default())
            .await
            .unwrap();
        cla.sink
            .get()
            .unwrap()
            .add_peer(
                cla::ClaAddress::Private("peer-a".as_bytes().into()),
                &[NodeId::Ipn(IpnNodeId {
                    allocator_id: 0,
                    node_number: 2,
                })],
            )
            .await
            .unwrap();

        let app = Arc::new(SendOnlyApp { sink: Once::new() });
        bpa.register_application(ServiceId::Ipn(42), app.clone())
            .await
            .unwrap();

        // Each registration above is a routing change that wakes the Waiting
        // poller; a poll still scanning when the failed bundle parks would
        // re-attempt it for a change that predates it. Let them finish.
        bpa.rib.poll_waiting_idle().await;

        let sent = app
            .sink
            .get()
            .unwrap()
            .send(
                "ipn:0.2.99".parse().unwrap(),
                Bytes::from_static(b"One shot"),
                Duration::from_secs(3600),
                None,
            )
            .await
            .unwrap();

        // the timeout only bounds a regression
        let attempt = timeout(Duration::from_secs(5), attempts_rx.recv_async())
            .await
            .expect("Timed out waiting for the forward attempt")
            .expect("Attempt channel closed");
        assert_eq!(attempt, sent);

        // The bundle is back in Waiting; with no routing or link event since,
        // no further attempt may occur. shutdown() is the barrier: it joins
        // the pools, and the CLA records every attempt synchronously inside
        // forward(), so any wrong re-attempt is in attempts_rx by the time it
        // returns. No quiet window is involved.
        bpa.shutdown().await;
        assert!(
            attempts_rx.is_empty(),
            "A synchronous failure must not re-attempt without a routing event"
        );
    }
}
