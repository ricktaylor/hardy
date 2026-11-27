use hardy_async::async_trait;
use hardy_bpv7::eid::NodeId;
#[cfg(feature = "instrument")]
use tracing::instrument;

// Aliased: the CLA and routing-agent kinds each have an erased `BoxFactory`.
use crate::{
    Arc,
    builder::BpaBuilder,
    cla::{
        self, Cla, ClaInit, Registration, factory::BoxFactory as ClaBoxFactory,
        registry::ClaRegistry,
    },
    dispatcher::Dispatcher,
    node_ids::NodeIds,
    otel_metrics,
    policy::FlowControllerFactory,
    routing::{self, Rib, RoutingAgent, agent::factory::BoxFactory as AgentBoxFactory},
    services::{self, registry::ServiceRegistry},
    storage::store::Store,
};

pub use self::registration::BpaRegistration;

mod registration;

/// The core Bundle Processing Agent (RFC 9171).
///
/// Holds references to the store, RIB, CLA/service registries, and
/// dispatcher. Construct via [`BpaBuilder`] (obtained from [`Bpa::builder()`]).
///
/// After construction, call [`start()`](Bpa::start) to begin processing and
/// [`shutdown()`](Bpa::shutdown) for ordered teardown.
pub struct Bpa {
    node_ids: Arc<NodeIds>,
    store: Arc<Store>,
    rib: Arc<Rib>,
    cla_registry: Arc<ClaRegistry>,
    service_registry: Arc<ServiceRegistry>,
    dispatcher: Arc<Dispatcher>,
}

impl Bpa {
    pub(crate) fn from_parts(
        node_ids: Arc<NodeIds>,
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

    // The CLA registry and the dispatcher it registers against, for the
    // registry's own unit tests: a factory running against its claim is
    // otherwise unobservable from there.
    #[cfg(test)]
    pub(crate) fn cla_registry(&self) -> (&Arc<ClaRegistry>, &Arc<Dispatcher>) {
        (&self.cla_registry, &self.dispatcher)
    }

    // The RIB, for the routing agent sink's unit tests: a factory that
    // fails is otherwise unreachable, because a configured one fails
    // `build()`.
    #[cfg(test)]
    pub(crate) fn rib(&self) -> &Arc<Rib> {
        &self.rib
    }

    // The service registry and what it registers against, for the same
    // purpose as `cla_registry`.
    #[cfg(test)]
    pub(crate) fn service_registry(
        &self,
    ) -> (&Arc<ServiceRegistry>, &NodeIds, &Arc<Rib>, &Arc<Dispatcher>) {
        (
            &self.service_registry,
            &self.node_ids,
            &self.rib,
            &self.dispatcher,
        )
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
        let factory = ClaBoxFactory::new(Registration::new(cla, init));
        self.cla_registry
            .register(name, factory, &self.dispatcher, policy)
            .await
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, service)))]
    async fn register_service_endpoints(
        &self,
        service_ids: &[hardy_bpv7::eid::Service],
        service: Arc<dyn services::Service>,
    ) -> services::Result<Vec<hardy_bpv7::eid::Eid>> {
        self.service_registry
            .register_service_endpoints(
                service_ids,
                service,
                &self.node_ids,
                &self.rib,
                &self.dispatcher,
            )
            .await
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, application)))]
    async fn register_application_endpoints(
        &self,
        service_ids: &[hardy_bpv7::eid::Service],
        application: Arc<dyn services::Application>,
    ) -> services::Result<Vec<hardy_bpv7::eid::Eid>> {
        self.service_registry
            .register_application_endpoints(
                service_ids,
                application,
                &self.node_ids,
                &self.rib,
                &self.dispatcher,
            )
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
        self.rib
            .register_agent(name, AgentBoxFactory::new(agent))
            .await
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

        async fn on_unregister(&self, _endpoint: &Eid) {}

        async fn on_deliver(
            &self,
            _endpoint: &Eid,
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
