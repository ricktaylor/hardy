//! Egress lanes: a peer keeps up to its effective lanes of forwards in
//! flight — one for a CLA declaring no lanes, otherwise its declaration
//! bounded by the BPA's egress seat cap, shared by all of the peer's policy
//! queues — pulling the next bundle as each lane frees, and a cap of one
//! forwards serially in queue order. Removing the peer lets its in-flight
//! forwards finish.

use core::num::{NonZeroU32, NonZeroU64};
use std::{
    borrow::Cow,
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicUsize, Ordering},
    },
};

use hardy_async::sync::spin::Once;
use hardy_bpa::{
    Bytes, async_trait,
    bpa::{Bpa, BpaRegistration},
    builder::BpaBuilder,
    cla::{self, ClaAddress, ClaInit},
    lanes::Lanes,
    node_ids::NodeIds,
    policy::{EgressQueue, EgressQueueSet, FlowController, FlowControllerFactory},
    stream::{Receiver, Segment},
};
use hardy_bpv7::{
    builder::Builder,
    bundle::Id,
    creation_timestamp::CreationTimestamp,
    eid::{IpnNodeId, NodeId},
};
use tokio::time::{Duration, timeout};

// flume's channel types stay qualified: their names collide with the
// imported `stream::Receiver`.

// The peer-owning CLA. Every forward announces itself, holds until the test
// releases one lane (or drops the release sender), then pulls its stream to
// completion and answers `Sent`. It records how many forwards were ever in
// flight at once.
struct HoldCla {
    sink: Once<Box<dyn cla::Sink>>,
    entered_tx: flume::Sender<Id>,
    release_rx: flume::Receiver<()>,
    finished_tx: flume::Sender<Id>,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
}

struct HoldHandles {
    entered_rx: flume::Receiver<Id>,
    release_tx: flume::Sender<()>,
    finished_rx: flume::Receiver<Id>,
}

impl HoldCla {
    fn new() -> (Arc<Self>, HoldHandles) {
        let (entered_tx, entered_rx) = flume::unbounded();
        let (release_tx, release_rx) = flume::unbounded();
        let (finished_tx, finished_rx) = flume::unbounded();
        (
            Arc::new(Self {
                sink: Once::new(),
                entered_tx,
                release_rx,
                finished_tx,
                in_flight: AtomicUsize::new(0),
                max_in_flight: AtomicUsize::new(0),
            }),
            HoldHandles {
                entered_rx,
                release_tx,
                finished_rx,
            },
        )
    }
}

#[async_trait]
impl cla::Cla for HoldCla {
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
        bundle_id: &Id,
        _total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> cla::Result<cla::ForwardBundleResult> {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(now, Ordering::SeqCst);
        let _ = self.entered_tx.send(bundle_id.clone());

        let _ = self.release_rx.recv_async().await;
        loop {
            match stream.recv().await {
                Ok(Segment::Final(_)) => break,
                Ok(Segment::Next(_)) => {}
                Err(_) => return Err(cla::Error::StreamCancelled),
            }
        }

        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        let _ = self.finished_tx.send(bundle_id.clone());
        Ok(cla::ForwardBundleResult::Sent)
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
        _cla_addr: &ClaAddress,
        _bundle_id: &Id,
        _total_len: u64,
        _stream: &mut dyn Receiver<Segment>,
    ) -> cla::Result<cla::ForwardBundleResult> {
        Ok(cla::ForwardBundleResult::Sent)
    }
}

const PEER_NODE: u32 = 3;

fn peer_addr() -> ClaAddress {
    ClaAddress::Private("peer".as_bytes().into())
}

// A BPA (node 1) with the hold CLA's peer (node 3) attached, declaring
// `lanes`, and an ingress CLA to inject bundles routed to it.
async fn rig(
    builder: BpaBuilder,
    lanes: Lanes,
) -> (Bpa, Arc<HoldCla>, HoldHandles, Arc<IngressCla>) {
    rig_with_policy(builder, lanes, None).await
}

// `rig`, with the hold CLA bound to `policy`.
async fn rig_with_policy(
    builder: BpaBuilder,
    lanes: Lanes,
    policy: Option<Arc<dyn FlowControllerFactory>>,
) -> (Bpa, Arc<HoldCla>, HoldHandles, Arc<IngressCla>) {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let bpa = builder.node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    let (cla, handles) = HoldCla::new();
    bpa.register_cla(
        "hold".to_string(),
        cla.clone(),
        policy,
        ClaInit {
            lanes,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    cla.sink
        .get()
        .unwrap()
        .add_peer(
            peer_addr(),
            &[NodeId::Ipn(IpnNodeId {
                allocator_id: 0,
                node_number: PEER_NODE,
            })],
        )
        .await
        .unwrap();

    let ingress = Arc::new(IngressCla { sink: Once::new() });
    bpa.register_cla(
        "ingress".to_string(),
        ingress.clone(),
        None,
        ClaInit::default(),
    )
    .await
    .unwrap();

    (bpa, cla, handles, ingress)
}

// Queue `count` bundles to the peer, in order. Each dispatch returns once
// its bundle is committed to the peer's egress queue.
async fn queue_bundles(ingress: &IngressCla, count: u32) -> Vec<Id> {
    let mut ids = Vec::new();
    for i in 0..count {
        let (bundle, data) = Builder::new(
            "ipn:0.2.1".parse().unwrap(),
            format!("ipn:0.{PEER_NODE}.{}", i + 1).parse().unwrap(),
        )
        .with_payload(Cow::Borrowed(b"egress lanes".as_slice()))
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

// Drives one peer through its lanes: `expected` forwards fill the
// lanes while every one is held, releasing a lane starts exactly the next
// bundle, and no more than `expected` were ever in flight.
async fn assert_effective_lanes(builder: BpaBuilder, lanes: Lanes, expected: usize) {
    let (bpa, cla, handles, ingress) = rig(builder, lanes).await;
    let ids = queue_bundles(&ingress, expected as u32 + 2).await;

    // Every lane fills while each forward is held inside the CLA.
    let mut entered = Vec::new();
    for _ in 0..expected {
        entered.push(recv(&handles.entered_rx, "a lane to fill").await);
    }
    assert_eq!(cla.in_flight.load(Ordering::SeqCst), expected);

    // Freeing one lane pulls exactly the next bundle.
    handles.release_tx.send(()).unwrap();
    recv(&handles.finished_rx, "the released forward").await;
    entered.push(recv(&handles.entered_rx, "the freed lane to refill").await);

    // Release everything and drain: shutdown joins the pools.
    drop(handles.release_tx);
    for _ in 1..ids.len() {
        recv(&handles.finished_rx, "the remaining forwards").await;
    }
    entered.push(recv(&handles.entered_rx, "the last bundle").await);
    bpa.shutdown().await;

    entered.sort();
    let mut queued = ids;
    queued.sort();
    assert_eq!(
        entered, queued,
        "every queued bundle is offered exactly once"
    );
    assert!(handles.entered_rx.is_empty(), "no bundle is offered twice");
    assert_eq!(
        cla.max_in_flight.load(Ordering::SeqCst),
        expected,
        "the peer never exceeds its effective lanes"
    );
}

// A CLA that declares nothing forwards to its peer one bundle at a time,
// however generous the cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cla_declaring_no_lanes_forwards_serially() {
    assert_effective_lanes(Bpa::builder(), ClaInit::default().lanes, 1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unlimited_cla_gets_the_egress_cap() {
    assert_effective_lanes(
        Bpa::builder().egress_lane_cap(NonZeroU32::new(3).unwrap()),
        Lanes::Unlimited,
        3,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_declared_lane_count_below_the_cap_bounds_the_queue() {
    assert_effective_lanes(
        Bpa::builder().egress_lane_cap(NonZeroU32::new(8).unwrap()),
        Lanes::Limited(NonZeroU32::new(2).unwrap()),
        2,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_egress_cap_bounds_a_larger_declared_lane_count() {
    assert_effective_lanes(
        Bpa::builder().egress_lane_cap(NonZeroU32::new(3).unwrap()),
        Lanes::Limited(NonZeroU32::new(5).unwrap()),
        3,
    )
    .await;
}

// The default cap lets a CLA declaring TCPCLv4's default sixteen lanes
// carry eight forwards per peer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_default_egress_cap_is_eight() {
    assert_effective_lanes(
        Bpa::builder(),
        Lanes::Limited(NonZeroU32::new(16).unwrap()),
        8,
    )
    .await;
}

// A cap of one forwards to the peer serially: one forward at a time, in
// queue order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_egress_cap_of_one_forwards_serially_in_queue_order() {
    let (bpa, cla, handles, ingress) = rig(
        Bpa::builder().egress_lane_cap(NonZeroU32::MIN),
        Lanes::Unlimited,
    )
    .await;
    let ids = queue_bundles(&ingress, 4).await;

    let mut entered = Vec::new();
    for _ in &ids {
        entered.push(recv(&handles.entered_rx, "the next forward").await);
        handles.release_tx.send(()).unwrap();
        recv(&handles.finished_rx, "the forward to finish").await;
    }
    bpa.shutdown().await;

    assert_eq!(entered, ids, "forwards start in queue order");
    assert_eq!(cla.max_in_flight.load(Ordering::SeqCst), 1);
}

// A policy with two queues that alternates its assignments, so a peer's
// bundles spread over both queues and both pollers draw on its lanes.
struct TwoQueuePolicy;

struct AlternatingController {
    next: AtomicU32,
    queue: Arc<dyn EgressQueue>,
}

#[async_trait]
impl FlowController for AlternatingController {
    fn queue_for(&self) -> u32 {
        self.next.fetch_add(1, Ordering::SeqCst) % 2
    }

    async fn forward(&self, _queue: u32, bundle: hardy_bpa::bundle::Bundle) {
        self.queue.forward(bundle).await
    }
}

#[async_trait]
impl FlowControllerFactory for TwoQueuePolicy {
    fn queue_count(&self) -> NonZeroU32 {
        NonZeroU32::new(2).unwrap()
    }

    async fn new_controller(&self, queues: EgressQueueSet) -> Arc<dyn FlowController> {
        Arc::new(AlternatingController {
            next: AtomicU32::new(0),
            queue: queues.next_free,
        })
    }
}

// The lanes are the peer's, not each queue's: under a two-queue policy, a
// CLA declaring two lanes still sees at most two forwards at once, and the
// queues share them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peers_queues_share_its_lanes() {
    let (bpa, cla, handles, ingress) = rig_with_policy(
        Bpa::builder(),
        Lanes::Limited(NonZeroU32::new(2).unwrap()),
        Some(Arc::new(TwoQueuePolicy)),
    )
    .await;
    let ids = queue_bundles(&ingress, 6).await;

    recv(&handles.entered_rx, "a lane to fill").await;
    recv(&handles.entered_rx, "the other lane to fill").await;
    assert_eq!(cla.in_flight.load(Ordering::SeqCst), 2);

    drop(handles.release_tx);
    for _ in 0..ids.len() {
        recv(&handles.finished_rx, "the forwards to finish").await;
    }
    bpa.shutdown().await;

    assert_eq!(
        cla.max_in_flight.load(Ordering::SeqCst),
        2,
        "two queues never exceed the peer's two lanes"
    );
}

// Removing the peer while every lane is busy: the in-flight forwards still
// run to completion — removal never cancels a transfer the CLA is mid-way
// through — and the bundles still queued are swept, never offered. (That
// the poller itself waits for its forwards is pinned in `cla::peers`.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removing_a_peer_completes_its_in_flight_forwards_and_offers_no_more() {
    let (bpa, cla, handles, ingress) = rig(
        Bpa::builder().egress_lane_cap(NonZeroU32::new(2).unwrap()),
        Lanes::Unlimited,
    )
    .await;
    queue_bundles(&ingress, 4).await;

    let mut in_flight = vec![
        recv(&handles.entered_rx, "the first lane to fill").await,
        recv(&handles.entered_rx, "the second lane to fill").await,
    ];

    assert!(
        cla.sink
            .get()
            .unwrap()
            .remove_peer(&peer_addr())
            .await
            .unwrap()
    );

    drop(handles.release_tx);
    let mut finished = vec![
        recv(&handles.finished_rx, "an in-flight forward to finish").await,
        recv(
            &handles.finished_rx,
            "the other in-flight forward to finish",
        )
        .await,
    ];
    bpa.shutdown().await;

    in_flight.sort();
    finished.sort();
    assert_eq!(
        finished, in_flight,
        "both in-flight forwards ran to completion"
    );
    assert!(
        handles.entered_rx.is_empty(),
        "a bundle swept from the closed queue is never offered"
    );
}
