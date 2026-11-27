//! `BpaBuilder`'s configured registrations: a CLA is configured as a
//! `cla::Factory` and built when the BPA starts, with its sink in the
//! `Context`; a duplicate name or a refused service id fails `build()` as a
//! typed `builder::Error` naming the component.

use core::num::NonZeroU64;
use std::sync::Arc;

use hardy_bpa::{
    async_trait,
    bpa::{Bpa, BpaRegistration},
    builder::Error,
    cla::{self, Cla, ClaAddress, ClaInit, Context, Factory, Registration, Terms},
    node_ids::NodeIds,
    routing::{self, AgentContext, AgentFactory, RouteAction, RoutingAgent, RoutingSink},
    services::{
        self, Application, ApplicationContext, ApplicationFactory, ApplicationSink, Service,
        ServiceContext, ServiceFactory, ServiceSink, StatusNotify,
    },
    stream::{Receiver, Segment},
};
use hardy_bpv7::{
    bundle::Id,
    eid::{DtnNodeId, Eid, IpnNodeId, NodeId, Service as ServiceId},
    status_report::ReasonCode,
};
use hardy_eid_patterns::EidPattern;
use time::OffsetDateTime;

/// Records the cap `on_register` delivers.
struct RecordingCla {
    sink: hardy_async::sync::spin::Once<Box<dyn cla::Sink>>,
    cap_tx: flume::Sender<Option<NonZeroU64>>,
}

impl RecordingCla {
    fn new(cap_tx: flume::Sender<Option<NonZeroU64>>) -> Arc<Self> {
        Arc::new(Self {
            sink: hardy_async::sync::spin::Once::new(),
            cap_tx,
        })
    }
}

#[async_trait]
impl Cla for RecordingCla {
    async fn on_register(
        &self,
        sink: Box<dyn cla::Sink>,
        _node_ids: &[NodeId],
        max_bundle_size: Option<NonZeroU64>,
    ) {
        self.sink.call_once(|| sink);
        let _ = self.cap_tx.send(max_bundle_size);
    }

    async fn on_unregister(&self) {}

    async fn forward(
        &self,
        _lane: Option<u32>,
        _cla_addr: &ClaAddress,
        _bundle_id: &Id,
        _total_len: u64,
        _stream: &mut dyn Receiver<Segment>,
    ) -> cla::Result<cla::ForwardBundleResult> {
        Ok(cla::ForwardBundleResult::Sent)
    }
}

/// A factory that builds its CLA from the `Context`: it sends what it was
/// handed out of `build`, then wraps the context's sink.
struct ContextFactory {
    init: ClaInit,
    built_tx: flume::Sender<(Terms, Arc<NodeIds>)>,
    sink_tx: flume::Sender<Arc<dyn cla::Sink>>,
    cap_tx: flume::Sender<Option<NonZeroU64>>,
}

impl Factory for ContextFactory {
    fn request(&self) -> ClaInit {
        self.init
    }

    fn build(self, cx: Context) -> cla::Result<Arc<dyn Cla>> {
        let _ = self.built_tx.send((cx.terms, cx.node_ids));
        let _ = self.sink_tx.send(cx.sink);
        Ok(RecordingCla::new(self.cap_tx))
    }
}

/// Refuses whatever terms it is offered.
struct RefusingFactory;

impl Factory for RefusingFactory {
    fn build(self, _cx: Context) -> cla::Result<Arc<dyn Cla>> {
        Err(cla::Error::Disconnected)
    }
}

fn declaring(max_bundle_size: Option<NonZeroU64>) -> ClaInit {
    ClaInit {
        max_bundle_size,
        ..Default::default()
    }
}

/// Records its registration EID.
struct RecordingService {
    eid_tx: flume::Sender<Eid>,
}

#[async_trait]
impl Service for RecordingService {
    async fn on_register(&self, endpoint: &Eid, _sink: Box<dyn ServiceSink>) {
        let _ = self.eid_tx.send(endpoint.clone());
    }

    async fn on_unregister(&self) {}

    async fn on_deliver(
        &self,
        _bundle_id: &Id,
        _expiry: OffsetDateTime,
        _total_len: u64,
        _stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        Ok(())
    }

    async fn on_status_notify(
        &self,
        _bundle_id: &Id,
        _from: &Eid,
        _kind: StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<OffsetDateTime>,
    ) {
    }
}

/// Records its registration EID.
struct RecordingApp {
    eid_tx: flume::Sender<Eid>,
}

#[async_trait]
impl Application for RecordingApp {
    async fn on_register(&self, source: &Eid, _sink: Box<dyn ApplicationSink>) {
        let _ = self.eid_tx.send(source.clone());
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
        _kind: StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<OffsetDateTime>,
    ) {
    }
}

/// A service factory that sends its context out of `build`.
struct ServiceContextFactory {
    built_tx: flume::Sender<(Eid, Arc<dyn ServiceSink>)>,
    eid_tx: flume::Sender<Eid>,
}

impl ServiceFactory for ServiceContextFactory {
    fn build(self, cx: ServiceContext) -> services::Result<Arc<dyn Service>> {
        let _ = self.built_tx.send((cx.endpoint, cx.sink));
        Ok(Arc::new(RecordingService {
            eid_tx: self.eid_tx,
        }))
    }
}

/// An application factory that sends its endpoint out of `build`.
struct ApplicationContextFactory {
    built_tx: flume::Sender<Eid>,
    eid_tx: flume::Sender<Eid>,
}

impl ApplicationFactory for ApplicationContextFactory {
    fn build(self, cx: ApplicationContext) -> services::Result<Arc<dyn Application>> {
        let _ = self.built_tx.send(cx.endpoint);
        Ok(Arc::new(RecordingApp {
            eid_tx: self.eid_tx,
        }))
    }
}

/// Records its registration and keeps its sink.
struct RecordingAgent {
    registered_tx: flume::Sender<()>,
    sink: hardy_async::sync::spin::Once<Box<dyn RoutingSink>>,
}

#[async_trait]
impl RoutingAgent for RecordingAgent {
    async fn on_register(&self, sink: Box<dyn RoutingSink>, _node_ids: &[NodeId]) {
        self.sink.call_once(|| sink);
        let _ = self.registered_tx.send(());
    }

    async fn on_unregister(&self) {}
}

/// An agent factory that sends its context out of `build`.
struct AgentContextFactory {
    built_tx: flume::Sender<(Arc<NodeIds>, Arc<dyn RoutingSink>)>,
    registered_tx: flume::Sender<()>,
}

impl AgentFactory for AgentContextFactory {
    fn build(self, cx: AgentContext) -> routing::Result<Arc<dyn RoutingAgent>> {
        let _ = self.built_tx.send((cx.node_ids, cx.sink));
        Ok(Arc::new(RecordingAgent {
            registered_tx: self.registered_tx,
            sink: hardy_async::sync::spin::Once::new(),
        }))
    }
}

fn service() -> (Arc<RecordingService>, flume::Receiver<Eid>) {
    let (eid_tx, eid_rx) = flume::bounded(1);
    (Arc::new(RecordingService { eid_tx }), eid_rx)
}

fn ipn_node_ids() -> NodeIds {
    [NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 7,
    })]
    .as_slice()
    .try_into()
    .unwrap()
}

// A configured factory runs at start(), not at build(): it receives the
// terms settled from its declarations against the BPA's cap, and the BPA's
// node ids, and on_register receives the same cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn factory_builds_at_start_against_the_settled_terms() {
    let (built_tx, built_rx) = flume::bounded(1);
    let (sink_tx, _sink_rx) = flume::bounded(1);
    let (cap_tx, cap_rx) = flume::bounded(1);
    let factory = ContextFactory {
        init: declaring(NonZeroU64::new(4096)),
        built_tx,
        sink_tx,
        cap_tx,
    };

    let bpa = Bpa::builder()
        .node_ids(ipn_node_ids())
        .max_bundle_size(NonZeroU64::new(1024).unwrap())
        .cla("configured", factory, None)
        .build()
        .await
        .unwrap();
    assert!(built_rx.is_empty(), "build() must not run the factory");

    bpa.start(false).await;
    let (terms, node_ids) = built_rx.try_recv().expect("start() runs the factory");
    assert_eq!(
        terms.max_bundle_size,
        NonZeroU64::new(1024),
        "the smaller cap wins"
    );
    assert_eq!(
        Vec::<NodeId>::from(&*node_ids),
        Vec::<NodeId>::from(&ipn_node_ids())
    );
    assert_eq!(cap_rx.try_recv().unwrap(), NonZeroU64::new(1024));
    bpa.shutdown().await;
}

// The sink in the Context is live once the CLA is registered, and the
// registration survives on_register's handle being the only other owner.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_sink_reaches_the_registration() {
    let (built_tx, _built_rx) = flume::bounded(1);
    let (sink_tx, sink_rx) = flume::bounded(1);
    let (cap_tx, _cap_rx) = flume::bounded(1);
    let factory = ContextFactory {
        init: ClaInit::default(),
        built_tx,
        sink_tx,
        cap_tx,
    };
    let bpa = Bpa::builder()
        .cla("configured", factory, None)
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    let sink = sink_rx.try_recv().expect("the factory kept its sink");
    let peer_addr = ClaAddress::Private("peer".into());
    let peer = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 9,
    });
    assert!(
        sink.add_peer(peer_addr.clone(), &[peer]).await.unwrap(),
        "the context's sink adds a peer for the registered CLA"
    );
    assert!(sink.remove_peer(&peer_addr).await.unwrap());

    bpa.shutdown().await;
    assert!(
        matches!(
            sink.add_peer(peer_addr, &[]).await,
            Err(cla::Error::Disconnected)
        ),
        "the sink is dead once the CLA is unregistered"
    );
}

// An already-built CLA is a factory through Registration: its declarations
// are folded the same way and reach on_register.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registration_is_a_factory_for_a_built_cla() {
    let (cap_tx, cap_rx) = flume::bounded(1);
    let registration =
        Registration::new(RecordingCla::new(cap_tx), declaring(NonZeroU64::new(4096)));
    let bpa = Bpa::builder()
        .cla("built", registration, None)
        .build()
        .await
        .unwrap();
    assert!(cap_rx.is_empty(), "nothing registers before start()");

    bpa.start(false).await;
    assert_eq!(
        cap_rx.try_recv().expect("start() registers the CLA"),
        NonZeroU64::new(4096)
    );
    bpa.shutdown().await;
}

// A factory refusing its terms leaves its name free: the next registration
// under it succeeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_terms_leave_the_name_free() {
    let (cap_tx, cap_rx) = flume::bounded(1);
    let bpa = Bpa::builder()
        .cla("refusing", RefusingFactory, None)
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    bpa.register_cla(
        "refusing".into(),
        RecordingCla::new(cap_tx),
        None,
        ClaInit::default(),
    )
    .await
    .expect("the refused build never claimed the name");
    assert!(cap_rx.try_recv().is_ok());
    bpa.shutdown().await;
}

#[tokio::test]
async fn duplicate_cla_name_is_an_error_not_a_panic() {
    let (cap_tx, _) = flume::bounded(2);
    let first = Registration::new(RecordingCla::new(cap_tx.clone()), ClaInit::default());
    let second = Registration::new(RecordingCla::new(cap_tx), ClaInit::default());
    let Err(err) = Bpa::builder()
        .cla("same", first, None)
        .cla("same", second, None)
        .build()
        .await
    else {
        panic!("a duplicate name must fail build()");
    };
    assert!(
        matches!(
            &err,
            Error::Cla { name, source: cla::Error::AlreadyExists(dup) } if name == "same" && dup == "same"
        ),
        "{err:?}"
    );
}

// A service and an application both go live at start(), on the EIDs their
// service ids resolve to.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_and_application_register_at_start() {
    let (service, service_rx) = service();
    let (app_tx, app_rx) = flume::bounded(1);

    let bpa = Bpa::builder()
        .node_ids(ipn_node_ids())
        .service(ServiceId::Ipn(1), service)
        .application(ServiceId::Ipn(2), Arc::new(RecordingApp { eid_tx: app_tx }))
        .build()
        .await
        .unwrap();
    assert!(service_rx.is_empty() && app_rx.is_empty());

    bpa.start(false).await;
    assert_eq!(
        service_rx.try_recv().unwrap(),
        "ipn:0.7.1".parse::<Eid>().unwrap()
    );
    assert_eq!(
        app_rx.try_recv().unwrap(),
        "ipn:0.7.2".parse::<Eid>().unwrap()
    );
    bpa.shutdown().await;
}

// A service and an application share one service-id space.
#[tokio::test]
async fn service_id_taken_by_an_application_is_refused() {
    let (service, _) = service();
    let (app_tx, _) = flume::bounded(1);

    let Err(err) = Bpa::builder()
        .node_ids(ipn_node_ids())
        .application(ServiceId::Ipn(1), Arc::new(RecordingApp { eid_tx: app_tx }))
        .service(ServiceId::Ipn(1), service)
        .build()
        .await
    else {
        panic!("a taken service id must fail build()");
    };
    let Error::Service { service_id, source } = &err else {
        panic!("expected Service, got {err:?}");
    };
    assert_eq!(*service_id, ServiceId::Ipn(1));
    assert!(
        matches!(**source, services::Error::ServiceIdInUse(_)),
        "{source:?}"
    );
}

#[tokio::test]
async fn administrative_service_id_is_refused() {
    let (service, _) = service();
    let Err(err) = Bpa::builder()
        .service(ServiceId::Ipn(0), service)
        .build()
        .await
    else {
        panic!("the administrative endpoint must fail build()");
    };
    assert!(
        matches!(
            &err,
            Error::Service { service_id: ServiceId::Ipn(0), source }
                if matches!(**source, services::Error::AdministrativeEndpoint(_))
        ),
        "{err:?}"
    );
}

// A dtn service id on a node with only an ipn node id has no EID to
// register on.
#[tokio::test]
async fn service_id_without_a_node_id_for_its_scheme_is_refused() {
    let (service, _) = service();
    let Err(err) = Bpa::builder()
        .node_ids(ipn_node_ids())
        .service(ServiceId::Dtn("echo".into()), service)
        .build()
        .await
    else {
        panic!("an unresolvable service id must fail build()");
    };
    let Error::Service { service_id, source } = &err else {
        panic!("expected Service, got {err:?}");
    };
    assert_eq!(*service_id, ServiceId::Dtn("echo".into()));
    assert!(matches!(**source, services::Error::NodeId(_)), "{source:?}");
}

// The dtn scheme resolves once a dtn node id is configured.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dtn_service_id_resolves_against_a_dtn_node_id() {
    let (service, service_rx) = service();
    let node_ids: NodeIds = [NodeId::Dtn(DtnNodeId {
        node_name: "node".into(),
    })]
    .as_slice()
    .try_into()
    .unwrap();

    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .service(ServiceId::Dtn("echo".into()), service)
        .build()
        .await
        .unwrap();
    bpa.start(false).await;
    assert_eq!(
        service_rx.try_recv().unwrap(),
        "dtn://node/echo".parse::<Eid>().unwrap()
    );
    bpa.shutdown().await;
}

// A service factory runs at start(), at the endpoint its service id
// resolves to, and the context's sink sends once the service is live.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_factory_builds_at_start_at_its_endpoint() {
    let (built_tx, built_rx) = flume::bounded(1);
    let (eid_tx, eid_rx) = flume::bounded(1);
    let bpa = Bpa::builder()
        .node_ids(ipn_node_ids())
        .service(
            ServiceId::Ipn(1),
            ServiceContextFactory { built_tx, eid_tx },
        )
        .build()
        .await
        .unwrap();
    assert!(built_rx.is_empty(), "build() must not run the factory");

    bpa.start(false).await;
    let (endpoint, sink) = built_rx.try_recv().expect("start() runs the factory");
    let expected: Eid = "ipn:0.7.1".parse().unwrap();
    assert_eq!(endpoint, expected);
    assert_eq!(eid_rx.try_recv().unwrap(), expected, "on_register follows");

    bpa.shutdown().await;
    let mut bundle = hardy_bpa::Bytes::new();
    assert!(
        matches!(
            sink.send(&mut bundle).await,
            Err(services::Error::Disconnected)
        ),
        "the context's sink is dead once the service is unregistered"
    );
}

// An application factory runs at start(), at its resolved endpoint.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn application_factory_builds_at_start_at_its_endpoint() {
    let (built_tx, built_rx) = flume::bounded(1);
    let (eid_tx, eid_rx) = flume::bounded(1);
    let bpa = Bpa::builder()
        .node_ids(ipn_node_ids())
        .application(
            ServiceId::Ipn(2),
            ApplicationContextFactory { built_tx, eid_tx },
        )
        .build()
        .await
        .unwrap();
    assert!(built_rx.is_empty(), "build() must not run the factory");

    bpa.start(false).await;
    let expected: Eid = "ipn:0.7.2".parse().unwrap();
    assert_eq!(built_rx.try_recv().unwrap(), expected);
    assert_eq!(eid_rx.try_recv().unwrap(), expected);
    bpa.shutdown().await;
}

// A routing agent factory is handed the node ids and a sink that installs
// routes once the agent is registered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_factory_sink_installs_routes() {
    let (built_tx, built_rx) = flume::bounded(1);
    let (registered_tx, registered_rx) = flume::bounded(1);
    let bpa = Bpa::builder()
        .node_ids(ipn_node_ids())
        .routing_agent(
            "configured",
            AgentContextFactory {
                built_tx,
                registered_tx,
            },
        )
        .build()
        .await
        .unwrap();
    let (node_ids, sink) = built_rx.try_recv().expect("build() runs the factory");
    assert_eq!(
        Vec::<NodeId>::from(&*node_ids),
        Vec::<NodeId>::from(&ipn_node_ids())
    );
    registered_rx.try_recv().expect("on_register follows");

    let pattern: EidPattern = "ipn:0.9.*".parse().unwrap();
    let action = RouteAction::Via("ipn:0.9.0".parse().unwrap());
    assert!(
        sink.add_route(pattern.clone(), action.clone(), 10)
            .await
            .unwrap()
    );
    assert!(sink.remove_route(&pattern, &action, 10).await.unwrap());
    bpa.shutdown().await;
}

// An agent built under a name already taken is dropped unfiled: build()
// refuses it, and its sink never acts for the agent holding the name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_agent_sink_never_acts_for_the_holder() {
    let (first_built_tx, _first_built_rx) = flume::bounded(1);
    let (first_registered_tx, _first_registered_rx) = flume::bounded(1);
    let (built_tx, built_rx) = flume::bounded(1);
    let (registered_tx, registered_rx) = flume::bounded(1);
    let Err(err) = Bpa::builder()
        .routing_agent(
            "same",
            AgentContextFactory {
                built_tx: first_built_tx,
                registered_tx: first_registered_tx,
            },
        )
        .routing_agent(
            "same",
            AgentContextFactory {
                built_tx,
                registered_tx,
            },
        )
        .build()
        .await
    else {
        panic!("a taken name must fail build()");
    };
    assert!(
        matches!(&err, Error::Routing(routing::Error::AlreadyExists(name)) if name == "same"),
        "{err:?}"
    );
    assert!(
        registered_rx.is_empty(),
        "the refused agent never registers"
    );

    let (_, sink) = built_rx.try_recv().expect("the second factory ran");
    let pattern: EidPattern = "ipn:0.9.*".parse().unwrap();
    assert!(
        matches!(
            sink.add_route(pattern, RouteAction::Drop(None), 10).await,
            Err(routing::Error::Disconnected)
        ),
        "the unfiled agent's sink must not act under the holder's name"
    );
}
