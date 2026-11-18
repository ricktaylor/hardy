use hardy_async::async_trait;
// `eid::Service` is the service number inside an EID, not the service
// trait registered here; it is aliased to the word the code already uses
// for it.
use hardy_bpv7::eid::Service as ServiceId;
#[cfg(feature = "instrument")]
use tracing::instrument;

// The CLA, service and routing agent tables are all `Table`; each is
// aliased to the subsystem it belongs to.
use crate::{
    Arc,
    builder::BpaBuilder,
    cla::{self, table::Table as ClaTable},
    dispatcher::Dispatcher,
    otel_metrics,
    routing::{self, Rib, agent::table::Table as AgentTable},
    service::{self, table::Table as ServiceTable},
    storage::store::Store,
};

/// The core Bundle Processing Agent (RFC 9171).
///
/// Holds the store, the RIB, the CLA table, the service table, the
/// routing agent table and the dispatcher. Construct via [`BpaBuilder`]
/// (obtained from [`Bpa::builder()`]).
///
/// After construction, call [`start()`](Bpa::start) to begin processing and
/// [`shutdown()`](Bpa::shutdown) for ordered teardown.
pub struct Bpa {
    store: Arc<Store>,
    rib: Arc<Rib>,
    clas: Arc<ClaTable>,
    services: Arc<ServiceTable>,
    agents: Arc<AgentTable>,
    dispatcher: Arc<Dispatcher>,
}

impl Bpa {
    pub(crate) fn from_parts(
        store: Arc<Store>,
        rib: Arc<Rib>,
        clas: Arc<ClaTable>,
        services: Arc<ServiceTable>,
        agents: Arc<AgentTable>,
        dispatcher: Arc<Dispatcher>,
    ) -> Self {
        Self {
            store,
            rib,
            clas,
            services,
            agents,
            dispatcher,
        }
    }

    // The CLA table, for that table's own unit tests: a factory that
    // runs under the table's lock is otherwise unobservable from here.
    #[cfg(test)]
    pub(crate) fn cla_table(&self) -> &Arc<ClaTable> {
        &self.clas
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

        // Only now start the declared components: the quiescent-store
        // precondition above covers the configuration path exactly as it
        // covers dynamic registrations. Start-up is teardown's mirror, so
        // the routing agents come up last of the three and go down first.
        let failures = self.clas.start().await;
        self.services.start().await;
        self.agents.start().await;

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

        self.agents.shutdown().await;
        self.clas.shutdown().await;
        self.services.shutdown().await;
        self.dispatcher.shutdown().await;
        self.rib.shutdown().await;
        self.store.shutdown().await;
    }
}

#[async_trait]
impl routing::Registrar for Bpa {
    #[cfg_attr(feature = "instrument", instrument(skip(self, factory)))]
    async fn register_routing_agent_boxed(
        &self,
        name: &str,
        factory: routing::BoxFactory<'_>,
    ) -> routing::Result<()> {
        self.agents.register(name, factory).await
    }
}

#[async_trait]
impl service::Registrar for Bpa {
    #[cfg_attr(feature = "instrument", instrument(skip(self, factory)))]
    async fn register_service_boxed(
        &self,
        id: Option<ServiceId>,
        factory: service::BoxFactory<'_>,
    ) -> service::Result<()> {
        self.services
            .register(id, service::table::FactoryKind::Payload(factory))
            .await
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, factory)))]
    async fn register_raw_service_boxed(
        &self,
        id: Option<ServiceId>,
        factory: service::raw::BoxFactory<'_>,
    ) -> service::Result<()> {
        self.services
            .register(id, service::table::FactoryKind::Raw(factory))
            .await
    }
}

#[async_trait]
impl cla::Registrar for Bpa {
    #[cfg_attr(feature = "instrument", instrument(skip(self, factory)))]
    async fn register_cla_boxed(
        &self,
        name: &str,
        factory: cla::BoxFactory<'_>,
    ) -> cla::Result<()> {
        self.clas.register(name, factory).await
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use hardy_bpv7::{
        bundle::Id,
        eid::{Eid, IpnNodeId, NodeId, Service as ServiceId},
        status_report::ReasonCode,
    };
    use time::OffsetDateTime;
    use tokio::time::timeout;

    use super::*;
    use crate::{
        Bytes,
        cla::RegistrarExt,
        service::RegistrarExt as _,
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
        sink: Arc<dyn service::Sink>,
    }

    impl SendOnlyService {
        fn factory() -> impl service::Factory<Component = Self> {
            service::factory_fn(|cx: service::Context| Ok(Arc::new(Self { sink: cx.sink })))
        }
    }

    #[async_trait]
    impl service::Service for SendOnlyService {
        async fn deliver(
            &self,
            _bundle_id: &Id,
            _expiry: OffsetDateTime,
            _ack_requested: bool,
            _total_len: u64,
            _stream: &mut dyn Receiver<Segment>,
        ) -> service::Result<()> {
            Ok(())
        }

        async fn report_status(
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

        let service = bpa
            .register_service(Some(ServiceId::Ipn(42)), SendOnlyService::factory())
            .await
            .unwrap();

        // Each registration above is a routing change that wakes the Waiting
        // poller; a poll still scanning when the failed bundle parks would
        // re-attempt it for a change that predates it. Let them finish.
        bpa.rib.poll_waiting_idle().await;

        let sent = service
            .sink
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
