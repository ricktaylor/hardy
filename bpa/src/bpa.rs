use hardy_async::async_trait;
// `eid::Service` is the service number inside an EID, not the service
// trait registered here; it is aliased to the word the code already uses
// for it.
use hardy_bpv7::eid::{Eid, NodeId, Service as ServiceId};
#[cfg(feature = "instrument")]
use tracing::instrument;

use crate::{
    Arc,
    builder::BpaBuilder,
    cla::{self, BoxFactory, table::Table},
    dispatcher::Dispatcher,
    node_ids::NodeIds,
    otel_metrics,
    routing::{self, Agent, Rib},
    service::{self, Service, raw, registry::ServiceRegistry},
    storage::store::Store,
};

/// Registering services and routing agents with a BPA.
///
/// The trait abstracts the registration interface, so a component works
/// against a local [`Bpa`] or against a remote BPA reached over gRPC.
/// CLAs come in through [`cla::Registrar`], a supertrait of this one,
/// where the BPA builds the component instead of being handed one: a CLA
/// is constructed from a [`cla::Factory`] and receives its sink in the
/// [`cla::Context`], so the lifecycle described here is the service and
/// routing agent one.
///
/// # Component lifecycle
///
/// A service or routing agent is built by its own crate, where
/// `new(&Config) -> Result<Self, Error>` validates the configuration
/// eagerly, so errors surface before registration. A `register_*` method
/// hands the component over and the BPA calls `on_register` with the sink
/// the component talks back through, which it then uses until it
/// unregisters: either by calling `sink.unregister()`, or by the BPA
/// shutting down and calling `on_unregister`.
///
/// # Sink storage
///
/// A component keeps its sink for its whole active lifetime, typically in
/// a `spin::Once`. An `on_register` that returns without storing the sink
/// has dropped it, and the BPA reads that as a request to unregister.
///
/// After unregistration the sink stays stored but answers
/// `Error::Disconnected` to everything, so `on_unregister` handles only
/// the component's own cleanup: stopping tasks, closing connections.
/// Defensive patterns such as an `Option<Sink>` taken in `on_unregister`
/// buy nothing.
///
/// # For implementors
///
/// A routing agent receives [`routing::Sink`] in
/// [`routing::Agent::on_register`], and maintains its routes with
/// `add_route` and `remove_route`; a fixed route set needs no trait
/// implementation at all, only [`routing::StaticAgent`]. A service
/// receives [`raw::Sink`] for whole bundles, or [`Sink`](service::Sink)
/// for payloads alone, in its own `on_register`.
#[async_trait]
pub trait BpaRegistration: cla::Registrar {
    /// Registers a payload-only service with the BPA.
    ///
    /// The service receives a [`Sink`](service::Sink) via
    /// [`Service::on_register`].
    async fn register_service(
        &self,
        service_id: ServiceId,
        service: Arc<dyn Service>,
    ) -> service::Result<Eid>;

    /// Registers a whole-bundle service with the BPA.
    ///
    /// The service receives a [`raw::Sink`] via
    /// [`raw::Service::on_register`].
    async fn register_raw_service(
        &self,
        service_id: ServiceId,
        service: Arc<dyn raw::Service>,
    ) -> service::Result<Eid>;

    /// Registers a payload-only service under a dynamically assigned service ID.
    async fn register_dynamic_service(&self, service: Arc<dyn Service>) -> service::Result<Eid>;

    /// Registers a whole-bundle service under a dynamically assigned service ID.
    async fn register_dynamic_raw_service(
        &self,
        service: Arc<dyn raw::Service>,
    ) -> service::Result<Eid>;

    /// Registers a routing agent with the BPA.
    ///
    /// The agent receives a [`routing::Sink`] via
    /// [`routing::Agent::on_register`] to manage routes in the RIB.
    ///
    /// `name` identifies this agent instance, and is the source recorded
    /// against every route it adds.
    ///
    /// Returns the BPA's node IDs.
    async fn register_routing_agent(
        &self,
        name: String,
        agent: Arc<dyn Agent>,
    ) -> routing::Result<Vec<NodeId>>;
}

/// The core Bundle Processing Agent (RFC 9171).
///
/// Holds the store, the RIB, the CLA table, the service registry and the
/// dispatcher. Construct via [`BpaBuilder`] (obtained from
/// [`Bpa::builder()`]).
///
/// After construction, call [`start()`](Bpa::start) to begin processing and
/// [`shutdown()`](Bpa::shutdown) for ordered teardown.
pub struct Bpa {
    node_ids: Arc<NodeIds>,
    store: Arc<Store>,
    rib: Arc<Rib>,
    clas: Arc<Table>,
    service_registry: Arc<ServiceRegistry>,
    dispatcher: Arc<Dispatcher>,
}

impl Bpa {
    pub(crate) fn from_parts(
        node_ids: Arc<NodeIds>,
        store: Arc<Store>,
        rib: Arc<Rib>,
        clas: Arc<Table>,
        service_registry: Arc<ServiceRegistry>,
        dispatcher: Arc<Dispatcher>,
    ) -> Self {
        Self {
            node_ids,
            store,
            rib,
            clas,
            service_registry,
            dispatcher,
        }
    }

    pub fn builder() -> BpaBuilder {
        BpaBuilder::new()
    }

    /// Starts the BPA's background machinery, reporting the CLAs that
    /// could not be started.
    ///
    /// When `recover_storage` is set, storage crash recovery runs to
    /// completion before this returns: recovery's checkpoint resets assume
    /// a quiescent store, so CLAs and services are only registered after
    /// `start` resolves.
    ///
    /// A declared CLA whose [`start`](cla::Cla::start) fails is detached
    /// and named in the report; the BPA runs on degraded rather than
    /// refusing to come up, since one dead transport is no reason to
    /// strand the bundles the others carry. An empty report means every
    /// declared CLA is live.
    ///
    /// Every failure is logged as it happens, so the report is for an
    /// embedder that takes a stronger line than running on degraded.
    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    pub async fn start(&self, recover_storage: bool) -> Vec<(String, cla::Error)> {
        otel_metrics::init();

        // Start the store, awaiting recovery so registrations that follow
        // cannot race the consistency check
        self.store
            .start(self.dispatcher.clone(), recover_storage)
            .await;

        // Only now start the declared CLAs and services: the
        // quiescent-store precondition above covers the configuration path
        // exactly as it covers dynamic registrations.
        let failures = self.clas.start().await;
        self.service_registry
            .start(&self.node_ids, &self.rib, &self.dispatcher)
            .await;

        // Start the RIB
        self.rib.start(self.dispatcher.clone());

        failures
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    pub async fn shutdown(&self) {
        // Shutdown order is critical for clean termination:
        //
        // 1. Routing agents - Remove dynamic routes (prevents new forwarding decisions)
        // 2. CLAs - Stop external bundle sources (network I/O)
        // 3. Services - Stop internal bundle sources (services calling sink.send())
        // 4. Dispatcher - Drain remaining in-flight bundles (all sources now closed)
        // 5. RIB - No more routing lookups needed
        // 6. Store - No more data access needed
        //
        // Routing agents shut down first so their routes are removed before CLAs
        // drain. CLAs and Services must shut down BEFORE dispatcher because they
        // are bundle sources. The dispatcher's processing pool may have tasks
        // blocked on CLA forwarding or waiting for service responses.

        self.rib.shutdown_agents().await;
        self.clas.shutdown().await;
        self.service_registry.shutdown(&self.rib).await;
        self.dispatcher.shutdown().await;
        self.rib.shutdown().await;
        self.store.shutdown().await;
    }
}

#[async_trait]
impl BpaRegistration for Bpa {
    #[cfg_attr(feature = "instrument", instrument(skip(self, service)))]
    async fn register_service(
        &self,
        service_id: ServiceId,
        service: Arc<dyn Service>,
    ) -> service::Result<Eid> {
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

    #[cfg_attr(feature = "instrument", instrument(skip(self, service)))]
    async fn register_raw_service(
        &self,
        service_id: ServiceId,
        service: Arc<dyn raw::Service>,
    ) -> service::Result<Eid> {
        self.service_registry
            .register_raw_service(
                service_id,
                service,
                &self.node_ids,
                &self.rib,
                &self.dispatcher,
            )
            .await
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, service)))]
    async fn register_dynamic_service(&self, service: Arc<dyn Service>) -> service::Result<Eid> {
        self.service_registry
            .register_dynamic_service(service, &self.node_ids, &self.rib, &self.dispatcher)
            .await
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, service)))]
    async fn register_dynamic_raw_service(
        &self,
        service: Arc<dyn raw::Service>,
    ) -> service::Result<Eid> {
        self.service_registry
            .register_dynamic_raw_service(service, &self.node_ids, &self.rib, &self.dispatcher)
            .await
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, agent)))]
    async fn register_routing_agent(
        &self,
        name: String,
        agent: Arc<dyn Agent>,
    ) -> routing::Result<Vec<NodeId>> {
        self.rib.register_agent(name, agent).await
    }
}

#[async_trait]
impl cla::Registrar for Bpa {
    #[cfg_attr(feature = "instrument", instrument(skip(self, factory)))]
    async fn register_cla_boxed(&self, name: &str, factory: BoxFactory<'_>) -> cla::Result<()> {
        self.clas.register(name, factory).await
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use hardy_async::sync::spin::Once;
    use hardy_bpv7::{
        bundle::Id,
        eid::{IpnNodeId, Service as ServiceId},
        status_report::ReasonCode,
    };
    use time::OffsetDateTime;
    use tokio::time::timeout;

    use super::*;
    use crate::{
        Bytes,
        cla::RegistrarExt,
        service::Sink,
        stream::{Receiver, Segment},
    };

    /// Fails every `forward` call synchronously, without pulling, recording
    /// the bundle each attempt was for.
    struct FailingCla {
        sink: Arc<dyn cla::Sink>,
        attempts_tx: flume::Sender<Id>,
    }

    impl FailingCla {
        fn factory(attempts_tx: flume::Sender<Id>) -> impl cla::Factory<Component = Self> {
            cla::factory_fn(cla::Request::default(), |cx: cla::Context| {
                Ok(Arc::new(Self {
                    sink: cx.sink,
                    attempts_tx,
                }))
            })
        }
    }

    #[async_trait]
    impl cla::Cla for FailingCla {
        async fn forward(
            &self,
            _lane: Option<u32>,
            _cla_addr: &cla::Address,
            bundle_id: &Id,
            _total_len: u64,
            _stream: &mut dyn Receiver<Segment>,
        ) -> cla::Result<cla::ForwardOutcome> {
            let _ = self.attempts_tx.send(bundle_id.clone());
            Err(cla::Error::StreamCancelled)
        }
    }

    /// Originates bundles; never has any delivered.
    struct SendOnlyService {
        sink: Once<Box<dyn Sink>>,
    }

    #[async_trait]
    impl Service for SendOnlyService {
        async fn on_register(&self, _source: &Eid, sink: Box<dyn Sink>) {
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
        ) -> service::Result<()> {
            Ok(())
        }

        async fn on_status_notify(
            &self,
            _bundle_id: &Id,
            _from: &Eid,
            _kind: service::StatusNotify,
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
        assert!(bpa.start(false).await.is_empty());

        let (attempts_tx, attempts_rx) = flume::unbounded();
        let cla = bpa
            .register_cla("failing", FailingCla::factory(attempts_tx))
            .await
            .unwrap();
        cla.sink
            .add_peer(
                cla::Address::Private("peer-a".as_bytes().into()),
                &[NodeId::Ipn(IpnNodeId {
                    allocator_id: 0,
                    node_number: 2,
                })],
            )
            .await
            .unwrap();

        let service = Arc::new(SendOnlyService { sink: Once::new() });
        bpa.register_service(ServiceId::Ipn(42), service.clone())
            .await
            .unwrap();

        // Each registration above is a routing change that wakes the Waiting
        // poller; a poll still scanning when the failed bundle parks would
        // re-attempt it for a change that predates it. Let them finish.
        bpa.rib.poll_waiting_idle().await;

        let sent = service
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
