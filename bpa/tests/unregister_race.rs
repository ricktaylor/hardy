//! CLA unregistration must close a peer's egress queues before sweeping
//! them: the `ForwardPending`/`ForwardAckPending` resets inside the RIB
//! withdrawal run once no new send can re-enter the queue, so a concurrent
//! forward holding a pre-withdrawal RIB snapshot either lands before the
//! sweep (and is swept) or bounces off the closed queue (and is parked
//! back to `Waiting` by its caller) — never stranded in `ForwardPending`
//! on a dead peer.

use core::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::{
    borrow::Cow,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use hardy_bpa::{
    Bytes, async_trait,
    bpa::{Bpa, BpaRegistration},
    bundle::{Bundle, BundleMetadata, BundleStatus},
    cla,
    node_ids::NodeIds,
    storage::{self, MetadataMemStorage, MetadataStorage},
    stream::{Receiver, Segment, Sender},
};
use hardy_bpv7::{
    builder::Builder,
    bundle::Id,
    creation_timestamp::CreationTimestamp,
    eid::{Eid, IpnNodeId, NodeId},
};

// ---------------------------------------------------------------------------
// A metadata store that gates the unregister sweeps and one queue entry
// ---------------------------------------------------------------------------

/// Delegates to [`MetadataMemStorage`], exposing three rendezvous points:
/// every commit into `ForwardPending` is notified; once armed, the next
/// `ForwardPending` swap parks at a gate until released; and the two peer
/// sweeps (`reset_peer_queue`, `reset_peer_ack_pending`) park at gates of
/// their own, so the test can interleave a forward with a precise point
/// inside `unregister`.
struct SweepGate {
    inner: MetadataMemStorage,
    /// Fires (bundle id) after any commit into `ForwardPending`.
    fp_notify_tx: flume::Sender<Id>,
    /// One-shot: the next `ForwardPending` swap after arming parks here.
    fp_armed: AtomicBool,
    fp_entered_tx: flume::Sender<()>,
    fp_release_rx: flume::Receiver<()>,
    /// One-shot gate inside `reset_peer_queue`.
    qs_gated: AtomicBool,
    qs_entered_tx: flume::Sender<()>,
    qs_release_rx: flume::Receiver<()>,
    qs_done_tx: flume::Sender<()>,
    /// One-shot gate inside `reset_peer_ack_pending`.
    ack_gated: AtomicBool,
    ack_entered_tx: flume::Sender<()>,
    ack_release_rx: flume::Receiver<()>,
}

struct SweepGateHandles {
    fp_notify_rx: flume::Receiver<Id>,
    fp_entered_rx: flume::Receiver<()>,
    fp_release_tx: flume::Sender<()>,
    qs_entered_rx: flume::Receiver<()>,
    qs_release_tx: flume::Sender<()>,
    qs_done_rx: flume::Receiver<()>,
    ack_entered_rx: flume::Receiver<()>,
    ack_release_tx: flume::Sender<()>,
}

impl SweepGate {
    fn new() -> (Arc<Self>, SweepGateHandles) {
        let (fp_notify_tx, fp_notify_rx) = flume::unbounded();
        let (fp_entered_tx, fp_entered_rx) = flume::unbounded();
        let (fp_release_tx, fp_release_rx) = flume::unbounded();
        let (qs_entered_tx, qs_entered_rx) = flume::unbounded();
        let (qs_release_tx, qs_release_rx) = flume::unbounded();
        let (qs_done_tx, qs_done_rx) = flume::unbounded();
        let (ack_entered_tx, ack_entered_rx) = flume::unbounded();
        let (ack_release_tx, ack_release_rx) = flume::unbounded();
        (
            Arc::new(Self {
                inner: MetadataMemStorage::new(None),
                fp_notify_tx,
                fp_armed: AtomicBool::new(false),
                fp_entered_tx,
                fp_release_rx,
                qs_gated: AtomicBool::new(true),
                qs_entered_tx,
                qs_release_rx,
                qs_done_tx,
                ack_gated: AtomicBool::new(true),
                ack_entered_tx,
                ack_release_rx,
            }),
            SweepGateHandles {
                fp_notify_rx,
                fp_entered_rx,
                fp_release_tx,
                qs_entered_rx,
                qs_release_tx,
                qs_done_rx,
                ack_entered_rx,
                ack_release_tx,
            },
        )
    }
}

#[async_trait]
impl MetadataStorage for SweepGate {
    async fn get(&self, bundle_id: &Id) -> storage::Result<Option<Bundle>> {
        self.inner.get(bundle_id).await
    }

    async fn insert(&self, bundle: &Bundle) -> storage::Result<bool> {
        self.inner.insert(bundle).await
    }

    async fn swap_status(
        &self,
        bundle_id: &Id,
        expected: &BundleStatus,
        status: &BundleStatus,
    ) -> storage::Result<bool> {
        if matches!(status, BundleStatus::ForwardPending { .. })
            && self.fp_armed.swap(false, Ordering::AcqRel)
        {
            let _ = self.fp_entered_tx.send(());
            let _ = self.fp_release_rx.recv_async().await;
        }
        let swapped = self.inner.swap_status(bundle_id, expected, status).await?;
        if swapped && matches!(status, BundleStatus::ForwardPending { .. }) {
            let _ = self.fp_notify_tx.send(bundle_id.clone());
        }
        Ok(swapped)
    }

    async fn tombstone_if(&self, bundle_id: &Id, expected: &BundleStatus) -> storage::Result<bool> {
        self.inner.tombstone_if(bundle_id, expected).await
    }

    async fn tombstone(&self, bundle_id: &Id) -> storage::Result<()> {
        self.inner.tombstone(bundle_id).await
    }

    async fn start_recovery(&self) {
        self.inner.start_recovery().await
    }

    async fn confirm_exists(
        &self,
        bundle_id: &Id,
    ) -> storage::Result<Option<(BundleMetadata, BundleStatus)>> {
        self.inner.confirm_exists(bundle_id).await
    }

    async fn remove_unconfirmed(&self, stream: &dyn Sender<Bundle>) -> storage::Result<()> {
        self.inner.remove_unconfirmed(stream).await
    }

    async fn reset_peer_queue(&self, peer: u32) -> storage::Result<u64> {
        if self.qs_gated.swap(false, Ordering::AcqRel) {
            let _ = self.qs_entered_tx.send(());
            let _ = self.qs_release_rx.recv_async().await;
            let reset = self.inner.reset_peer_queue(peer).await;
            let _ = self.qs_done_tx.send(());
            return reset;
        }
        self.inner.reset_peer_queue(peer).await
    }

    async fn reset_peer_ack_pending(&self, peer: u32) -> storage::Result<u64> {
        if self.ack_gated.swap(false, Ordering::AcqRel) {
            let _ = self.ack_entered_tx.send(());
            let _ = self.ack_release_rx.recv_async().await;
        }
        self.inner.reset_peer_ack_pending(peer).await
    }

    async fn reset_service_queue(&self, service: &Eid) -> storage::Result<u64> {
        self.inner.reset_service_queue(service).await
    }

    async fn poll_expiry(&self, stream: &dyn Sender<Bundle>) -> storage::Result<()> {
        self.inner.poll_expiry(stream).await
    }

    async fn poll_waiting(&self, stream: &dyn Sender<Bundle>) -> storage::Result<()> {
        self.inner.poll_waiting(stream).await
    }

    async fn poll_service_waiting(
        &self,
        source: Eid,
        stream: &dyn Sender<Bundle>,
    ) -> storage::Result<()> {
        self.inner.poll_service_waiting(source, stream).await
    }

    async fn poll_adu_fragments(
        &self,
        stream: &dyn Sender<Bundle>,
        status: &BundleStatus,
    ) -> storage::Result<()> {
        self.inner.poll_adu_fragments(stream, status).await
    }

    async fn poll_pending(
        &self,
        stream: &dyn Sender<Bundle>,
        status: &BundleStatus,
        limit: usize,
    ) -> storage::Result<()> {
        self.inner.poll_pending(stream, status, limit).await
    }
}

// ---------------------------------------------------------------------------
// Mock CLAs
// ---------------------------------------------------------------------------

/// The peer-owning CLA: its first `forward` parks on a rendezvous, keeping
/// the egress queue consumer busy so later sends queue behind it.
struct StallCla {
    sink: hardy_async::sync::spin::Once<Box<dyn cla::Sink>>,
    forward_entered_tx: flume::Sender<Id>,
    forward_release_rx: flume::Receiver<()>,
}

impl StallCla {
    fn new() -> (Arc<Self>, flume::Receiver<Id>, flume::Sender<()>) {
        let (forward_entered_tx, forward_entered_rx) = flume::unbounded();
        let (forward_release_tx, forward_release_rx) = flume::unbounded();
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                forward_entered_tx,
                forward_release_rx,
            }),
            forward_entered_rx,
            forward_release_tx,
        )
    }
}

#[async_trait]
impl cla::Cla for StallCla {
    async fn on_register(
        &self,
        sink: Box<dyn cla::Sink>,
        _node_ids: &[NodeId],
        _max_bundle_size: NonZeroU64,
    ) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    fn lane_count(&self) -> Option<NonZeroU32> {
        None
    }

    async fn forward(
        &self,
        _lane: Option<u32>,
        _cla_addr: &cla::ClaAddress,
        bundle_id: &Id,
        _total_len: u64,
        _stream: &mut dyn Receiver<Segment>,
    ) -> cla::Result<cla::ForwardBundleResult> {
        let _ = self.forward_entered_tx.send(bundle_id.clone());
        let _ = self.forward_release_rx.recv_async().await;
        Ok(cla::ForwardBundleResult::Sent)
    }
}

/// Minimal CLA to inject inbound bundles.
struct IngressCla {
    sink: hardy_async::sync::spin::Once<Box<dyn cla::Sink>>,
}

impl IngressCla {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            sink: hardy_async::sync::spin::Once::new(),
        })
    }
}

#[async_trait]
impl cla::Cla for IngressCla {
    async fn on_register(
        &self,
        sink: Box<dyn cla::Sink>,
        _node_ids: &[NodeId],
        _max_bundle_size: NonZeroU64,
    ) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    fn lane_count(&self) -> Option<NonZeroU32> {
        None
    }

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

// ---------------------------------------------------------------------------

/// Event-driven wait; the timeout only bounds a regression.
async fn recv<T>(rx: &flume::Receiver<T>, what: &str) -> T {
    tokio::time::timeout(tokio::time::Duration::from_secs(10), rx.recv_async())
        .await
        .unwrap_or_else(|_| panic!("Timed out waiting for {what}"))
        .unwrap_or_else(|_| panic!("Channel gone waiting for {what}"))
}

fn build_bundle(source: &str, destination: &str) -> (Id, Bytes) {
    let (bundle, data) = Builder::new(source.parse().unwrap(), destination.parse().unwrap())
        .with_payload(Cow::Borrowed(b"payload".as_slice()))
        .build(CreationTimestamp::now())
        .expect("Failed to build bundle");
    (bundle.primary.id, Bytes::from(data))
}

/// A forward that raced `unregister_cla` — its RIB snapshot taken before
/// the withdrawal, its queue entry committed after the peer sweep ran —
/// must not strand the bundle in `ForwardPending` on the dead peer: it
/// ends in `Waiting`, recoverable by the next route event.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_forward_is_not_stranded_by_unregister() {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let (metadata_store, gates) = SweepGate::new();
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .metadata_storage(metadata_store.clone())
        .poll_channel_depth(NonZeroUsize::new(1).unwrap())
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    let (cla, forward_entered_rx, forward_release_tx) = StallCla::new();
    bpa.register_cla("stall".to_string(), cla.clone(), None, None)
        .await
        .unwrap();
    cla.sink
        .get()
        .unwrap()
        .add_peer(
            cla::ClaAddress::Private("peer".as_bytes().into()),
            &[NodeId::Ipn(IpnNodeId {
                allocator_id: 0,
                node_number: 3,
            })],
        )
        .await
        .unwrap();

    let ingress = IngressCla::new();
    bpa.register_cla("ingress".to_string(), ingress.clone(), None, None)
        .await
        .unwrap();

    // Bundle A occupies the egress consumer: it is claimed out of the queue
    // and parked inside the CLA's forward.
    let (a_id, mut a_data) = build_bundle("ipn:0.2.1", "ipn:0.3.1");
    // Routing happens at the ingress gate and the forward executes on the
    // dispatching task, so a dispatch whose transfer stalls must not be
    // awaited inline: spawn it, as a real CLA session task would.
    let a_dispatch = {
        let ingress = ingress.clone();
        tokio::spawn(async move {
            assert_eq!(
                ingress
                    .sink
                    .get()
                    .unwrap()
                    .dispatch(None, None, &mut a_data)
                    .await
                    .expect("dispatch failed"),
                cla::Acceptance::Accepted
            );
        })
    };
    assert_eq!(recv(&gates.fp_notify_rx, "A queued").await, a_id);
    assert_eq!(recv(&forward_entered_rx, "A offered").await, a_id);

    // Bundle C fills the depth-1 channel buffer behind the busy consumer,
    // so the racing bundle's send below takes the storage-spill path.
    let (c_id, mut c_data) = build_bundle("ipn:0.2.1", "ipn:0.3.2");
    let c_dispatch = {
        let ingress = ingress.clone();
        tokio::spawn(async move {
            assert_eq!(
                ingress
                    .sink
                    .get()
                    .unwrap()
                    .dispatch(None, None, &mut c_data)
                    .await
                    .expect("dispatch failed"),
                cla::Acceptance::Accepted
            );
        })
    };
    assert_eq!(recv(&gates.fp_notify_rx, "C queued").await, c_id);

    // Bundle B is the racing forward: its RIB lookup resolves the live
    // peer, then its queue-entry swap parks at the gate.
    metadata_store.fp_armed.store(true, Ordering::Release);
    let (b_id, mut b_data) = build_bundle("ipn:0.2.1", "ipn:0.3.3");
    let b_dispatch = {
        let ingress = ingress.clone();
        tokio::spawn(async move {
            assert_eq!(
                ingress
                    .sink
                    .get()
                    .unwrap()
                    .dispatch(None, None, &mut b_data)
                    .await
                    .expect("dispatch failed"),
                cla::Acceptance::Accepted
            );
        })
    };
    recv(&gates.fp_entered_rx, "B's queue entry to park at the gate").await;

    // Unregister the CLA concurrently; it parks inside its peer sweeps.
    let unregister = {
        let cla = cla.clone();
        tokio::spawn(async move { cla.sink.get().unwrap().unregister().await })
    };
    recv(&gates.qs_entered_rx, "the ForwardPending sweep").await;

    // Run the sweep to completion, then land B's queue entry strictly
    // after it, while the second sweep gate still holds unregister open.
    gates.qs_release_tx.send(()).unwrap();
    recv(&gates.qs_done_rx, "the ForwardPending sweep to finish").await;
    recv(&gates.ack_entered_rx, "the ForwardAckPending sweep").await;
    gates.fp_release_tx.send(()).unwrap();
    assert_eq!(
        recv(&gates.fp_notify_rx, "B's queue entry to commit").await,
        b_id,
        "the racing forward must have entered ForwardPending after the sweep"
    );

    // Let unregister finish: close (already done, or about to happen —
    // order under test), RIB withdrawal, second sweep.
    gates.ack_release_tx.send(()).unwrap();
    unregister.await.unwrap();

    // Release the stalled transfer and drain everything: shutdown joins
    // the pools, so every park below has committed by the time it returns.
    forward_release_tx.send(()).unwrap();
    a_dispatch.await.unwrap();
    b_dispatch.await.unwrap();
    c_dispatch.await.unwrap();
    bpa.shutdown().await;

    let b = metadata_store
        .get(&b_id)
        .await
        .unwrap()
        .expect("racing bundle lost");
    assert_eq!(
        b.status,
        BundleStatus::Waiting,
        "a forward racing unregister must end recoverable in Waiting, not stranded on the dead peer"
    );
    let c = metadata_store
        .get(&c_id)
        .await
        .unwrap()
        .expect("queued bundle lost");
    assert_eq!(
        c.status,
        BundleStatus::Waiting,
        "a bundle queued before unregister is swept to Waiting"
    );
}
