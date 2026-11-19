use hardy_async::{async_trait, sync::spin::Mutex};
// `eid::Service` is a service number, not the service trait registered
// here, so it is aliased to the word the rest of the code uses for it.
use hardy_bpv7::eid::Service as ServiceId;
#[cfg(feature = "instrument")]
use tracing::instrument;

// The CLA, service and routing agent tables are all named `Table`, so
// each is aliased to the subsystem it belongs to.
use crate::{
    Arc,
    builder::BpaBuilder,
    cla::{self, table::Table as ClaTable},
    dispatcher::Dispatcher,
    otel_metrics,
    routing::{self, Rib, agent::table::Table as AgentTable},
    service::{
        self,
        table::{FactoryKind, Table as ServiceTable},
    },
    storage::store::Store,
};

/// The component factories an embedder declared on the [`BpaBuilder`],
/// each held until [`Bpa::start`] registers what it builds.
#[derive(Default)]
pub(crate) struct Factories {
    pub clas: Vec<(String, cla::BoxFactory<'static>)>,
    pub services: Vec<(Option<ServiceId>, FactoryKind<'static>)>,
    pub agents: Vec<(String, routing::BoxFactory<'static>)>,
}

/// The core Bundle Processing Agent (RFC 9171).
///
/// The BPA owns the store, the RIB, the CLA, service and routing agent
/// tables, and the dispatcher. It is built with a [`BpaBuilder`], from
/// [`Bpa::builder`]; [`start`](Self::start) brings it up, and
/// [`shutdown`](Self::shutdown) takes it down in order.
pub struct Bpa {
    store: Arc<Store>,
    rib: Arc<Rib>,
    clas: Arc<ClaTable>,
    services: Arc<ServiceTable>,
    agents: Arc<AgentTable>,
    dispatcher: Arc<Dispatcher>,
    // Emptied by `start`, which hands each factory to the table that holds
    // what it builds.
    factories: Mutex<Factories>,
}

impl Bpa {
    pub(crate) fn from_parts(
        store: Arc<Store>,
        rib: Arc<Rib>,
        clas: Arc<ClaTable>,
        services: Arc<ServiceTable>,
        agents: Arc<AgentTable>,
        dispatcher: Arc<Dispatcher>,
        factories: Factories,
    ) -> Self {
        Self {
            store,
            rib,
            clas,
            services,
            agents,
            dispatcher,
            factories: Mutex::new(factories),
        }
    }

    // The crate's tests have no other way to reach the table.
    #[cfg(test)]
    pub(crate) fn cla_table(&self) -> &Arc<ClaTable> {
        &self.clas
    }

    pub fn builder() -> BpaBuilder {
        BpaBuilder::new()
    }

    /// Starts the BPA's background machinery and registers every declared
    /// component.
    ///
    /// When `recover_storage` is set, storage crash recovery runs to
    /// completion before anything registers, since recovery's checkpoint
    /// resets assume a quiescent store.
    ///
    /// # Errors
    ///
    /// Returns the error of the first declared component that fails to
    /// register. The components registered before it are running, so an
    /// embedder that carries on rather than exiting takes the BPA down
    /// with [`shutdown`](Self::shutdown).
    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    pub async fn start(
        &self,
        recover_storage: bool,
    ) -> Result<(), Box<dyn core::error::Error + Send + Sync>> {
        otel_metrics::init();

        // Await recovery, so that the registrations that follow cannot
        // race the consistency check.
        self.store
            .start(self.dispatcher.clone(), recover_storage)
            .await;

        // A declared component registers exactly as one registered while
        // the BPA runs. Registration mirrors teardown, so the routing
        // agents come up last and go down first.
        let factories = core::mem::take(&mut *self.factories.lock());
        for (name, factory) in factories.clas {
            self.clas.register(&name, factory).await?;
        }
        for (id, factory) in factories.services {
            self.services.register(id, factory).await?;
        }
        for (name, factory) in factories.agents {
            self.agents.register(&name, factory).await?;
        }

        self.rib.start(self.dispatcher.clone());

        Ok(())
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    pub async fn shutdown(&self) {
        // Order matters. The routing agents go first, so that their routes
        // are gone before the CLAs drain. The CLAs and the services are the
        // bundle sources, so both stop before the dispatcher, whose pool
        // may hold tasks blocked on a CLA forward or a service delivery.
        // The RIB and the store are last, with nothing left to read them.
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

    // Fails every `forward` call synchronously, without pulling, and
    // records the bundle each attempt was for.
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

    // Originates bundles, and never has one delivered.
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

    // A synchronous per-transfer failure parks that bundle and nothing
    // else, with no inline retry: a deterministic failure must not spin
    // dispatch, forward, fail, so exactly one attempt occurs until the next
    // routing or link event.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_forward_does_not_retry_inline() {
        let bpa = Bpa::builder().build();
        bpa.start(false).await.unwrap();

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

        // Each registration above is a routing change that wakes the
        // Waiting poller, and a poll still scanning when the failed bundle
        // parks would re-attempt it for a change that predates it. Let the
        // polls finish.
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
