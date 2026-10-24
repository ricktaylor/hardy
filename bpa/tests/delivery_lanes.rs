//! Delivery lanes: a service's delivery queue keeps up to its effective
//! lanes of deliveries in flight — the lane count it declared at
//! registration, one by default, bounded by the BPA's delivery seat cap. A
//! service that declares nothing is delivered to one bundle at a time, in
//! queue order. Unregistering with deliveries in flight lets them finish
//! and sweeps the bundles still queued to `WaitingForService`.

use core::num::{NonZeroU32, NonZeroU64};
use std::{
    borrow::Cow,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use hardy_async::sync::spin::Once;
use hardy_bpa::{
    Bytes, async_trait,
    bpa::{Bpa, BpaRegistration},
    builder::BpaBuilder,
    bundle::BundleStatus,
    cla::{self, Lanes},
    node_ids::NodeIds,
    services,
    storage::{MetadataMemStorage, MetadataStorage},
    stream::{Receiver, Segment},
};
use hardy_bpv7::{
    builder::Builder,
    bundle::Id,
    creation_timestamp::CreationTimestamp,
    eid::{Eid, IpnNodeId, NodeId, Service},
    status_report::ReasonCode,
};
use time::OffsetDateTime;
use tokio::time::{Duration, timeout};

// flume's channel types stay qualified: their names collide with the
// imported `stream::Receiver`.

// Every delivery announces itself, holds until the test releases one lane
// (or drops the release sender), then pulls its stream to completion. The
// service records how many deliveries were ever in flight at once.
struct HoldService {
    sink: Once<Box<dyn services::ServiceSink>>,
    entered_tx: flume::Sender<Id>,
    release_rx: flume::Receiver<()>,
    delivered_tx: flume::Sender<Id>,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
}

struct HoldHandles {
    entered_rx: flume::Receiver<Id>,
    release_tx: flume::Sender<()>,
    delivered_rx: flume::Receiver<Id>,
}

impl HoldService {
    fn new() -> (Arc<Self>, HoldHandles) {
        let (entered_tx, entered_rx) = flume::unbounded();
        let (release_tx, release_rx) = flume::unbounded();
        let (delivered_tx, delivered_rx) = flume::unbounded();
        (
            Arc::new(Self {
                sink: Once::new(),
                entered_tx,
                release_rx,
                delivered_tx,
                in_flight: AtomicUsize::new(0),
                max_in_flight: AtomicUsize::new(0),
            }),
            HoldHandles {
                entered_rx,
                release_tx,
                delivered_rx,
            },
        )
    }
}

#[async_trait]
impl services::Service for HoldService {
    async fn on_register(&self, _endpoint: &Eid, sink: Box<dyn services::ServiceSink>) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn on_deliver(
        &self,
        bundle_id: &Id,
        _expiry: OffsetDateTime,
        _total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(now, Ordering::SeqCst);
        let _ = self.entered_tx.send(bundle_id.clone());

        let _ = self.release_rx.recv_async().await;
        loop {
            match stream.recv().await {
                Ok(Segment::Final(_)) => break,
                Ok(Segment::Next(_)) => {}
                Err(_) => return Err(services::Error::StreamCancelled),
            }
        }

        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        let _ = self.delivered_tx.send(bundle_id.clone());
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

// Injects inbound bundles; never forwards.
struct IngressCla {
    sink: Once<Box<dyn cla::Sink>>,
}

#[async_trait]
impl cla::Cla for IngressCla {
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
        _bundle_id: &Id,
        _total_len: u64,
        _stream: &mut dyn Receiver<Segment>,
    ) -> cla::Result<cla::ForwardBundleResult> {
        Ok(cla::ForwardBundleResult::Sent)
    }
}

const SERVICE: u32 = 7;

// A BPA (node 1) with the hold service registered at ipn:0.1.7 under
// `lanes`, and an ingress CLA to deliver bundles to it.
async fn rig(
    builder: BpaBuilder,
    lanes: Lanes,
) -> (
    Bpa,
    Arc<MetadataMemStorage>,
    Arc<HoldService>,
    HoldHandles,
    Arc<IngressCla>,
) {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let metadata_store = Arc::new(MetadataMemStorage::new(None));
    let bpa = builder
        .node_ids(node_ids)
        .metadata_storage(metadata_store.clone())
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    let (svc, handles) = HoldService::new();
    bpa.register_service(Service::Ipn(SERVICE), svc.clone(), lanes)
        .await
        .unwrap();

    let ingress = Arc::new(IngressCla { sink: Once::new() });
    bpa.register_cla(
        "ingress".to_string(),
        ingress.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();

    (bpa, metadata_store, svc, handles, ingress)
}

// Queue `count` bundles to the service, in order. Each dispatch returns
// once its bundle is committed to the service's delivery queue.
async fn queue_bundles(ingress: &IngressCla, count: u32) -> Vec<Id> {
    let mut ids = Vec::new();
    for _ in 0..count {
        let (bundle, data) = Builder::new(
            "ipn:0.2.1".parse().unwrap(),
            format!("ipn:0.1.{SERVICE}").parse().unwrap(),
        )
        .with_payload(Cow::Borrowed(b"delivery lanes".as_slice()))
        .build(CreationTimestamp::now())
        .expect("Failed to build bundle");
        assert_eq!(
            ingress
                .sink
                .get()
                .unwrap()
                .dispatch(None, None, &mut Bytes::from(data))
                .await
                .unwrap(),
            cla::Acceptance::Accepted
        );
        ids.push(bundle.primary.id);
    }
    ids
}

// Event-driven wait; the timeout only bounds a regression.
async fn recv<T>(rx: &flume::Receiver<T>, what: &str) -> T {
    timeout(Duration::from_secs(10), rx.recv_async())
        .await
        .unwrap_or_else(|_| panic!("Timed out waiting for {what}"))
        .unwrap_or_else(|_| panic!("Channel gone waiting for {what}"))
}

fn lanes(n: u32) -> Lanes {
    Lanes::Limited(NonZeroU32::new(n).unwrap())
}

// Drives one service's delivery queue through its lanes: `expected`
// deliveries fill the lanes while every one is held, releasing a lane
// starts exactly the next bundle, and no more than `expected` were ever in
// flight.
async fn assert_effective_lanes(builder: BpaBuilder, lanes: Lanes, expected: usize) {
    let (bpa, _store, svc, handles, ingress) = rig(builder, lanes).await;
    let ids = queue_bundles(&ingress, expected as u32 + 2).await;

    let mut entered = Vec::new();
    for _ in 0..expected {
        entered.push(recv(&handles.entered_rx, "a lane to fill").await);
    }
    assert_eq!(svc.in_flight.load(Ordering::SeqCst), expected);

    // Freeing one lane pulls exactly the next bundle.
    handles.release_tx.send(()).unwrap();
    recv(&handles.delivered_rx, "the released delivery").await;
    entered.push(recv(&handles.entered_rx, "the freed lane to refill").await);

    // Release everything and drain: shutdown joins the pools.
    drop(handles.release_tx);
    for _ in 1..ids.len() {
        recv(&handles.delivered_rx, "the remaining deliveries").await;
    }
    entered.push(recv(&handles.entered_rx, "the last bundle").await);
    bpa.shutdown().await;

    entered.sort();
    let mut queued = ids;
    queued.sort();
    assert_eq!(
        entered, queued,
        "every queued bundle is delivered exactly once"
    );
    assert!(
        handles.entered_rx.is_empty(),
        "no bundle is delivered twice"
    );
    assert_eq!(
        svc.max_in_flight.load(Ordering::SeqCst),
        expected,
        "the service never exceeds its effective lanes"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_service_declaring_lanes_gets_concurrent_deliveries() {
    assert_effective_lanes(Bpa::builder(), lanes(3), 3).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_delivery_cap_bounds_a_larger_declared_lane_count() {
    assert_effective_lanes(
        Bpa::builder().delivery_lane_cap(NonZeroU32::new(2).unwrap()),
        lanes(5),
        2,
    )
    .await;
}

// The default cap is eight: a service with no limit of its own declares
// unlimited lanes and gets eight.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_default_delivery_cap_is_eight() {
    assert_effective_lanes(Bpa::builder(), Lanes::Unlimited, 8).await;
}

// A service that declares nothing is delivered to serially, in queue
// order, however high the cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_service_declaring_nothing_is_delivered_to_serially() {
    let (bpa, _store, svc, handles, ingress) = rig(
        Bpa::builder().delivery_lane_cap(NonZeroU32::new(8).unwrap()),
        Lanes::Serial,
    )
    .await;
    let ids = queue_bundles(&ingress, 4).await;

    let mut entered = Vec::new();
    for _ in &ids {
        entered.push(recv(&handles.entered_rx, "the next delivery").await);
        handles.release_tx.send(()).unwrap();
        recv(&handles.delivered_rx, "the delivery to finish").await;
    }
    bpa.shutdown().await;

    assert_eq!(entered, ids, "deliveries start in queue order");
    assert_eq!(svc.max_in_flight.load(Ordering::SeqCst), 1);
}

// A cap of one delivers serially even to a service that declared more.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delivery_cap_of_one_delivers_serially() {
    assert_effective_lanes(
        Bpa::builder().delivery_lane_cap(NonZeroU32::MIN),
        lanes(4),
        1,
    )
    .await;
}

// Unregistering while every lane holds a delivery: the queued bundle is
// swept to WaitingForService and never reaches the service, and the
// in-flight deliveries keep their claims and complete normally.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unregister_lets_in_flight_deliveries_finish_and_sweeps_the_queue() {
    let (bpa, store, svc, handles, ingress) = rig(Bpa::builder(), lanes(2)).await;
    let ids = queue_bundles(&ingress, 3).await;

    let mut in_flight = vec![
        recv(&handles.entered_rx, "the first lane to fill").await,
        recv(&handles.entered_rx, "the second lane to fill").await,
    ];
    in_flight.sort();
    let queued = ids
        .iter()
        .find(|id| !in_flight.contains(id))
        .expect("one bundle is still queued")
        .clone();
    let service_eid: Eid = format!("ipn:0.1.{SERVICE}").parse().unwrap();
    for id in &in_flight {
        assert_eq!(
            store.get(id).await.unwrap().expect("bundle").status,
            BundleStatus::DeliveryAckPending {
                service: service_eid.clone()
            }
        );
    }

    // The sweep runs inside unregister, so the queued bundle is parked by
    // the time it returns.
    svc.sink.get().unwrap().unregister().await;
    assert_eq!(
        store.get(&queued).await.unwrap().expect("bundle").status,
        BundleStatus::WaitingForService {
            service: service_eid.clone()
        }
    );

    drop(handles.release_tx);
    let mut delivered = vec![
        recv(&handles.delivered_rx, "an in-flight delivery to finish").await,
        recv(
            &handles.delivered_rx,
            "the other in-flight delivery to finish",
        )
        .await,
    ];
    bpa.shutdown().await;

    delivered.sort();
    assert_eq!(delivered, in_flight, "both in-flight deliveries completed");
    assert!(
        handles.entered_rx.is_empty(),
        "the swept bundle never reaches the unregistered service"
    );
}
