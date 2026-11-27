//! One Service or Application registered on several endpoints: each
//! endpoint is its own registration with its own sink, a delivery names the
//! endpoint it arrived at, unregistration is per endpoint, and a multi-id
//! registration is all or none.

use core::{num::NonZeroU64, time::Duration};
use std::{borrow::Cow, collections::HashMap, sync::Arc};

use hardy_async::sync::spin::{Mutex, Once};
use hardy_bpa::{
    Bytes, async_trait,
    bpa::{Bpa, BpaRegistration},
    cla::{self, Cla, ClaAddress, ClaInit},
    node_ids::NodeIds,
    services::{self, Application, ApplicationSink, Service, ServiceSink, StatusNotify},
    stream::{Receiver, Segment, buffer_stream},
};
use hardy_bpv7::{
    builder::Builder,
    bundle::Id,
    creation_timestamp::CreationTimestamp,
    eid::{Eid, IpnNodeId, NodeId, Service as ServiceId},
    parse::{Parsed, parse},
    status_report::ReasonCode,
};
use time::OffsetDateTime;
use tokio::time::timeout;

/// What a multi-endpoint service observes, in order.
#[derive(Debug, PartialEq)]
enum Event {
    Registered(Eid),
    Delivered { endpoint: Eid, destination: Eid },
    Unregistered(Eid),
}

/// Records every call, keeping one sink per endpoint.
struct MultiService {
    sinks: Mutex<HashMap<Eid, Arc<dyn ServiceSink>>>,
    events: flume::Sender<Event>,
}

impl MultiService {
    fn new() -> (Arc<Self>, flume::Receiver<Event>) {
        let (events, rx) = flume::unbounded();
        (
            Arc::new(Self {
                sinks: Mutex::new(HashMap::new()),
                events,
            }),
            rx,
        )
    }

    fn sink(&self, endpoint: &Eid) -> Arc<dyn ServiceSink> {
        self.sinks
            .lock()
            .get(endpoint)
            .cloned()
            .expect("the endpoint has a sink")
    }
}

#[async_trait]
impl Service for MultiService {
    async fn on_register(&self, endpoint: &Eid, sink: Box<dyn ServiceSink>) {
        self.sinks.lock().insert(endpoint.clone(), Arc::from(sink));
        let _ = self.events.send(Event::Registered(endpoint.clone()));
    }

    async fn on_unregister(&self, endpoint: &Eid) {
        let _ = self.events.send(Event::Unregistered(endpoint.clone()));
    }

    async fn on_deliver(
        &self,
        endpoint: &Eid,
        _bundle_id: &Id,
        _expiry: OffsetDateTime,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        let data = buffer_stream(stream, total_len).await?;
        let Parsed { bundle, .. } = parse(data).expect("the BPA delivers a valid bundle");
        let _ = self.events.send(Event::Delivered {
            endpoint: endpoint.clone(),
            destination: bundle.primary.destination,
        });
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

/// Records each endpoint an application is registered and delivered on.
struct MultiApp {
    sinks: Mutex<HashMap<Eid, Arc<dyn ApplicationSink>>>,
    events: flume::Sender<Event>,
}

#[async_trait]
impl Application for MultiApp {
    async fn on_register(&self, source: &Eid, sink: Box<dyn ApplicationSink>) {
        self.sinks.lock().insert(source.clone(), Arc::from(sink));
        let _ = self.events.send(Event::Registered(source.clone()));
    }

    async fn on_unregister(&self, endpoint: &Eid) {
        let _ = self.events.send(Event::Unregistered(endpoint.clone()));
    }

    async fn on_deliver(
        &self,
        endpoint: &Eid,
        _bundle_id: &Id,
        _expiry: OffsetDateTime,
        _ack_requested: bool,
        _total_len: u64,
        _stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        // An application sees only the payload: the endpoint argument is
        // the only way it can tell its endpoints apart.
        let _ = self.events.send(Event::Delivered {
            endpoint: endpoint.clone(),
            destination: endpoint.clone(),
        });
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

/// Dispatches inbound bundles and records forwarded ones.
struct LoopCla {
    sink: Once<Box<dyn cla::Sink>>,
    forwarded: flume::Sender<Bytes>,
}

#[async_trait]
impl Cla for LoopCla {
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
        _cla_addr: &ClaAddress,
        _bundle_id: &Id,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> cla::Result<cla::ForwardBundleResult> {
        let _ = self.forwarded.send(buffer_stream(stream, total_len).await?);
        Ok(cla::ForwardBundleResult::Sent)
    }
}

const NODE: u32 = 7;
const PEER: u32 = 2;

fn node_ids() -> NodeIds {
    [NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: NODE,
    })]
    .as_slice()
    .try_into()
    .unwrap()
}

fn eid(node: u32, service: u32) -> Eid {
    format!("ipn:0.{node}.{service}").parse().unwrap()
}

fn bundle(source: &Eid, destination: &Eid) -> Bytes {
    let (_, data) = Builder::new(source.clone(), destination.clone())
        .with_payload(Cow::Borrowed(b"ping"))
        .build(CreationTimestamp::now())
        .unwrap();
    Bytes::from(data)
}

// A started BPA with a CLA whose peer is node `PEER`.
async fn started_bpa() -> (Bpa, Arc<LoopCla>, flume::Receiver<Bytes>) {
    let bpa = Bpa::builder().node_ids(node_ids()).build().await.unwrap();
    bpa.start(false).await;
    let (forwarded, forwarded_rx) = flume::unbounded();
    let cla = Arc::new(LoopCla {
        sink: Once::new(),
        forwarded,
    });
    bpa.register_cla("loop".into(), cla.clone(), None, ClaInit::default())
        .await
        .unwrap();
    cla.sink
        .get()
        .unwrap()
        .add_peer(
            ClaAddress::Private("peer".as_bytes().into()),
            &[NodeId::Ipn(IpnNodeId {
                allocator_id: 0,
                node_number: PEER,
            })],
        )
        .await
        .unwrap();
    (bpa, cla, forwarded_rx)
}

async fn next_event(events: &flume::Receiver<Event>) -> Event {
    // the timeout only bounds a regression
    timeout(Duration::from_secs(5), events.recv_async())
        .await
        .expect("timed out waiting for a service event")
        .expect("the service is alive")
}

// Each endpoint gets its own on_register, the EIDs come back in order, and
// a bundle to each endpoint is delivered naming that endpoint.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_endpoint_registers_and_receives_on_its_own() {
    let (bpa, cla, _) = started_bpa().await;
    let (service, events) = MultiService::new();

    let eids = bpa
        .register_service_endpoints(&[ServiceId::Ipn(1), ServiceId::Ipn(2)], service.clone())
        .await
        .unwrap();
    assert_eq!(eids, [eid(NODE, 1), eid(NODE, 2)]);
    assert_eq!(next_event(&events).await, Event::Registered(eid(NODE, 1)));
    assert_eq!(next_event(&events).await, Event::Registered(eid(NODE, 2)));

    for target in [eid(NODE, 2), eid(NODE, 1)] {
        let mut data = bundle(&eid(PEER, 9), &target);
        let acceptance = cla
            .sink
            .get()
            .unwrap()
            .dispatch(None, None, &mut data)
            .await
            .unwrap();
        assert_eq!(acceptance, cla::Acceptance::Accepted);
        assert_eq!(
            next_event(&events).await,
            Event::Delivered {
                endpoint: target.clone(),
                destination: target,
            }
        );
    }
    bpa.shutdown().await;
}

// A sink originates only from its own endpoint: a bundle sourced from the
// other endpoint is refused, one from its own is forwarded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_sink_sends_only_from_its_endpoint() {
    let (bpa, _cla, forwarded) = started_bpa().await;
    let (service, events) = MultiService::new();
    bpa.register_service_endpoints(&[ServiceId::Ipn(1), ServiceId::Ipn(2)], service.clone())
        .await
        .unwrap();
    next_event(&events).await;
    next_event(&events).await;

    let sink = service.sink(&eid(NODE, 1));
    let mut foreign = bundle(&eid(NODE, 2), &eid(PEER, 9));
    let Err(err) = sink.send(&mut foreign).await else {
        panic!("a bundle sourced from another endpoint must be refused");
    };
    assert!(
        matches!(&err, services::Error::InvalidDestination(source) if *source == eid(NODE, 2)),
        "{err:?}"
    );

    let mut own = bundle(&eid(NODE, 1), &eid(PEER, 9));
    sink.send(&mut own).await.unwrap();
    // the timeout only bounds a regression
    let sent = timeout(Duration::from_secs(5), forwarded.recv_async())
        .await
        .expect("timed out waiting for the forward")
        .unwrap();
    let Parsed { bundle, .. } = parse(sent).unwrap();
    assert_eq!(bundle.primary.id.source, eid(NODE, 1));
    bpa.shutdown().await;
}

// Unregistering one endpoint leaves the others registered, and names the
// endpoint that went away.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unregistering_one_endpoint_leaves_the_others() {
    let (bpa, _cla, _) = started_bpa().await;
    let (service, events) = MultiService::new();
    bpa.register_service_endpoints(&[ServiceId::Ipn(1), ServiceId::Ipn(2)], service.clone())
        .await
        .unwrap();
    next_event(&events).await;
    next_event(&events).await;

    service.sink(&eid(NODE, 1)).unregister().await;
    assert_eq!(next_event(&events).await, Event::Unregistered(eid(NODE, 1)));

    // ipn service 1 is free again; 2 is still taken.
    let (other, _) = MultiService::new();
    bpa.register_service(ServiceId::Ipn(1), other.clone())
        .await
        .expect("the unregistered endpoint is free");
    let Err(err) = bpa.register_service(ServiceId::Ipn(2), other).await else {
        panic!("the remaining endpoint must still be registered");
    };
    assert!(matches!(err, services::Error::ServiceIdInUse(_)), "{err:?}");

    // shutdown() is the barrier: it unregisters what remains, so every event
    // is in the channel once it returns.
    bpa.shutdown().await;
    let remaining: Vec<_> = events.drain().collect();
    assert_eq!(remaining, [Event::Unregistered(eid(NODE, 2))]);
}

// A taken id refuses the whole set: no endpoint registers, and nothing
// reaches the service.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_taken_id_registers_none_of_the_set() {
    let (bpa, _cla, _) = started_bpa().await;
    let (first, _) = MultiService::new();
    bpa.register_service(ServiceId::Ipn(2), first)
        .await
        .unwrap();

    let (service, events) = MultiService::new();
    let Err(err) = bpa
        .register_service_endpoints(&[ServiceId::Ipn(1), ServiceId::Ipn(2)], service)
        .await
    else {
        panic!("a taken id must fail the whole set");
    };
    assert!(
        matches!(&err, services::Error::ServiceIdInUse(id) if id == "2"),
        "{err:?}"
    );

    // ipn service 1 was not registered either.
    let (other, _) = MultiService::new();
    bpa.register_service(ServiceId::Ipn(1), other)
        .await
        .expect("no endpoint of the refused set was registered");

    bpa.shutdown().await;
    assert!(
        events.is_empty(),
        "the refused service saw {:?}",
        events.drain().collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn a_repeated_id_is_refused() {
    let bpa = Bpa::builder().node_ids(node_ids()).build().await.unwrap();
    bpa.start(false).await;
    let (service, _) = MultiService::new();
    let Err(err) = bpa
        .register_service_endpoints(&[ServiceId::Ipn(3), ServiceId::Ipn(3)], service)
        .await
    else {
        panic!("a repeated id must fail the set");
    };
    assert!(matches!(err, services::Error::ServiceIdInUse(_)), "{err:?}");
    bpa.shutdown().await;
}

#[tokio::test]
async fn an_empty_set_is_refused() {
    let bpa = Bpa::builder().node_ids(node_ids()).build().await.unwrap();
    bpa.start(false).await;
    let (service, _) = MultiService::new();
    let Err(err) = bpa.register_service_endpoints(&[], service).await else {
        panic!("a registration needs an id");
    };
    assert!(matches!(err, services::Error::NoServiceIds), "{err:?}");
    bpa.shutdown().await;
}

// An application on several endpoints learns which one each payload
// arrived at.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_application_learns_the_endpoint_of_each_delivery() {
    let (bpa, cla, _) = started_bpa().await;
    let (events_tx, events) = flume::unbounded();
    let app = Arc::new(MultiApp {
        sinks: Mutex::new(HashMap::new()),
        events: events_tx,
    });
    bpa.register_application_endpoints(&[ServiceId::Ipn(4), ServiceId::Ipn(5)], app)
        .await
        .unwrap();
    next_event(&events).await;
    next_event(&events).await;

    let mut data = bundle(&eid(PEER, 9), &eid(NODE, 5));
    let _ = cla
        .sink
        .get()
        .unwrap()
        .dispatch(None, None, &mut data)
        .await
        .unwrap();
    assert_eq!(
        next_event(&events).await,
        Event::Delivered {
            endpoint: eid(NODE, 5),
            destination: eid(NODE, 5),
        }
    );
    bpa.shutdown().await;
}

// The builder configures one service on several endpoints, registered at
// start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_builder_registers_one_service_on_several_endpoints() {
    let (service, events) = MultiService::new();
    let bpa = Bpa::builder()
        .node_ids(node_ids())
        .service_endpoints(
            [ServiceId::Ipn(1), ServiceId::Ipn(2)],
            service as Arc<dyn Service>,
        )
        .build()
        .await
        .unwrap();
    assert!(events.is_empty(), "nothing registers before start()");

    bpa.start(false).await;
    assert_eq!(next_event(&events).await, Event::Registered(eid(NODE, 1)));
    assert_eq!(next_event(&events).await, Event::Registered(eid(NODE, 2)));
    bpa.shutdown().await;
}

// Two configured registrations may not share an id: the clash names the
// second registration's whole set.
#[tokio::test]
async fn the_builder_refuses_an_id_shared_between_registrations() {
    let (first, _) = MultiService::new();
    let (second, _) = MultiService::new();
    let Err(err) = Bpa::builder()
        .node_ids(node_ids())
        .service(ServiceId::Ipn(2), first as Arc<dyn Service>)
        .service_endpoints(
            [ServiceId::Ipn(1), ServiceId::Ipn(2)],
            second as Arc<dyn Service>,
        )
        .build()
        .await
    else {
        panic!("a shared id must fail build()");
    };
    let hardy_bpa::builder::Error::Service {
        service_ids,
        source,
    } = &err
    else {
        panic!("expected Service, got {err:?}");
    };
    assert_eq!(*service_ids, [ServiceId::Ipn(1), ServiceId::Ipn(2)]);
    assert!(
        matches!(**source, services::Error::ServiceIdInUse(_)),
        "{source:?}"
    );
    assert_eq!(
        err.to_string(),
        "Failed to register service 1, 2: There is already a service registered as 2"
    );
}
