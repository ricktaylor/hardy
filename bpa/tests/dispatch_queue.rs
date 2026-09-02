//! Dispatch-queue duplicate suppression: the hybrid storage channel is
//! at-least-once, so its poller may re-push a copy of a bundle that the
//! consumer is already processing. The consumer must claim each dequeued
//! bundle out of `DispatchPending` and drop every copy that loses the swap,
//! or a slow service sees the same bundle delivered concurrently more than
//! once.

use core::{num::NonZeroU32, time::Duration};
use std::{
    borrow::Cow,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use hardy_bpa::{
    Bytes, async_trait,
    bpa::{Bpa, BpaRegistration},
    bundle::{Bundle, BundleMetadata, BundleStatus},
    cla,
    node_ids::NodeIds,
    services,
    storage::{self, MetadataMemStorage, MetadataStorage},
    stream::{Receiver, Segment, SendError, Sender},
};
use hardy_bpv7::{
    builder::Builder,
    bundle::Id,
    creation_timestamp::CreationTimestamp,
    eid::{Eid, IpnNodeId, NodeId, Service},
    status_report::ReasonCode,
};

// ---------------------------------------------------------------------------
// A metadata store that replays the poller-duplicate race
// ---------------------------------------------------------------------------

/// Collects a poll into a `Vec` so the injecting wrapper can replay it.
#[derive(Default)]
struct VecSink(Mutex<Vec<Bundle>>);

impl VecSink {
    fn into_inner(self) -> Vec<Bundle> {
        self.0.into_inner().unwrap()
    }
}

#[async_trait]
impl Sender<Bundle> for VecSink {
    async fn send(&self, item: Bundle) -> core::result::Result<(), SendError<Bundle>> {
        self.0.lock().unwrap().push(item);
        Ok(())
    }
}

/// Delegates to [`MetadataMemStorage`], holding the dispatch channel's
/// initial recovery poll open until armed, then answering it with each
/// queued bundle **twice** — the second copy being exactly the stale
/// duplicate the at-least-once channel contract permits the poller to
/// produce while the first copy is in flight.
struct InjectingStorage {
    inner: MetadataMemStorage,
    /// Fires when a bundle lands in `DispatchPending` (the channel send's swap).
    queued_tx: flume::Sender<()>,
    /// The initial `DispatchPending` poll blocks here until the test arms it,
    /// so the bundle under test verifiably rides the storage slow path.
    arm_rx: flume::Receiver<()>,
    /// Fires after the armed poll has pushed the duplicate copies.
    injected_tx: flume::Sender<()>,
    injected: AtomicBool,
}

impl InjectingStorage {
    fn new() -> (
        Arc<Self>,
        flume::Receiver<()>,
        flume::Sender<()>,
        flume::Receiver<()>,
    ) {
        let (queued_tx, queued_rx) = flume::unbounded();
        let (arm_tx, arm_rx) = flume::unbounded();
        let (injected_tx, injected_rx) = flume::unbounded();
        (
            Arc::new(Self {
                inner: MetadataMemStorage::new(None),
                queued_tx,
                arm_rx,
                injected_tx,
                injected: AtomicBool::new(false),
            }),
            queued_rx,
            arm_tx,
            injected_rx,
        )
    }
}

#[async_trait]
impl MetadataStorage for InjectingStorage {
    async fn get(&self, bundle_id: &Id) -> storage::Result<Option<Bundle>> {
        self.inner.get(bundle_id).await
    }

    async fn insert(&self, bundle: &Bundle) -> storage::Result<bool> {
        self.inner.insert(bundle).await
    }

    async fn update_status(&self, bundle_id: &Id, status: &BundleStatus) -> storage::Result<()> {
        self.inner.update_status(bundle_id, status).await
    }

    async fn swap_status(
        &self,
        bundle_id: &Id,
        expected: &BundleStatus,
        status: &BundleStatus,
    ) -> storage::Result<bool> {
        let swapped = self.inner.swap_status(bundle_id, expected, status).await?;
        if swapped && *status == BundleStatus::DispatchPending {
            let _ = self.queued_tx.send(());
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
        self.inner.reset_peer_queue(peer).await
    }

    async fn reset_peer_ack_pending(&self, peer: u32) -> storage::Result<u64> {
        self.inner.reset_peer_ack_pending(peer).await
    }

    async fn reset_service_queue(&self, service: &Eid) -> storage::Result<u64> {
        self.inner.reset_service_queue(service).await
    }

    async fn poll_expiry(&self, stream: &dyn Sender<Bundle>, limit: usize) -> storage::Result<()> {
        self.inner.poll_expiry(stream, limit).await
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
        if *status == BundleStatus::DispatchPending && !self.injected.load(Ordering::SeqCst) {
            let _ = self.arm_rx.recv_async().await;
            self.injected.store(true, Ordering::SeqCst);

            let collected = VecSink::default();
            self.inner.poll_pending(&collected, status, limit).await?;
            for bundle in collected.into_inner() {
                let duplicate = bundle.clone();
                if stream.send(bundle).await.is_err() || stream.send(duplicate).await.is_err() {
                    break;
                }
            }
            let _ = self.injected_tx.send(());
            return Ok(());
        }
        self.inner.poll_pending(stream, status, limit).await
    }
}

// ---------------------------------------------------------------------------
// A service that holds every delivery open and counts invocations
// ---------------------------------------------------------------------------

/// Holds each delivery open until the release sender is dropped, so the
/// bundle under test stays in flight while the poller injects its duplicate.
struct CountingHoldService {
    sink: hardy_async::sync::spin::Once<Box<dyn services::ServiceSink>>,
    deliveries: AtomicUsize,
    started_tx: flume::Sender<()>,
    release_rx: flume::Receiver<()>,
}

impl CountingHoldService {
    fn new() -> (Arc<Self>, flume::Receiver<()>, flume::Sender<()>) {
        let (started_tx, started_rx) = flume::unbounded();
        let (release_tx, release_rx) = flume::bounded(1);
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                deliveries: AtomicUsize::new(0),
                started_tx,
                release_rx,
            }),
            started_rx,
            release_tx,
        )
    }
}

#[async_trait]
impl services::Service for CountingHoldService {
    async fn on_register(&self, _endpoint: &Eid, sink: Box<dyn services::ServiceSink>) {
        // Retain the sink: dropping it unregisters the service.
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn on_deliver(
        &self,
        _bundle_id: &Id,
        _expiry: time::OffsetDateTime,
        _total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        self.deliveries.fetch_add(1, Ordering::SeqCst);
        let _ = self.started_tx.send(());
        // Released by channel closure, so a wrongly-delivered duplicate
        // completes and is counted rather than hanging the test.
        let _ = self.release_rx.recv_async().await;
        loop {
            match stream.recv().await {
                Ok(Segment::Final(_)) | Err(_) => break,
                Ok(_) => {}
            }
        }
        Ok(())
    }

    async fn on_status_notify(
        &self,
        _bundle_id: &Id,
        _from: &Eid,
        _kind: services::StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<time::OffsetDateTime>,
    ) {
    }
}

// ---------------------------------------------------------------------------
// Minimal CLA to inject the inbound bundle
// ---------------------------------------------------------------------------

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
        _max_bundle_size: core::num::NonZeroU64,
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

/// A stale queued-status copy re-pushed by the channel's storage poller
/// while the bundle is mid-delivery must lose the consumer's dequeue claim:
/// the service sees exactly one delivery.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_poller_duplicate_never_redelivers() {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let (metadata_store, queued_rx, arm_tx, injected_rx) = InjectingStorage::new();
    // Deliveries run on per-service channels, not the dispatch processing
    // pool, so the held delivery cannot starve the marker's; a duplicate
    // that wrongly survived the dispatch dequeue claim would queue behind
    // the held delivery and be counted when it is released below.
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .metadata_storage(metadata_store)
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    // The service under test holds its deliveries open; the marker service
    // completes immediately (its release sender is dropped at creation).
    let (svc, started_rx, release_tx) = CountingHoldService::new();
    bpa.register_service(Service::Ipn(7), svc.clone())
        .await
        .unwrap();
    let (marker_svc, marker_started_rx, marker_release_tx) = CountingHoldService::new();
    drop(marker_release_tx);
    bpa.register_service(Service::Ipn(8), marker_svc.clone())
        .await
        .unwrap();

    let (_, data) = Builder::new("ipn:0.2.1".parse().unwrap(), "ipn:0.1.7".parse().unwrap())
        .with_lifetime(Duration::from_secs(3600))
        .with_payload(Cow::Borrowed(b"deliver me once".as_slice()))
        .build(CreationTimestamp::now())
        .expect("Failed to build bundle");

    let cla = IngressCla::new();
    bpa.register_cla("ingress".to_string(), cla.clone(), None, None)
        .await
        .unwrap();
    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(None, None, &mut Bytes::from(data))
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // The dispatch send parks the bundle in DispatchPending on the storage
    // slow path — the initial recovery poll is still blocked on the arm
    // signal, keeping the channel's fast path closed. (Every timeout below
    // only bounds a regression.)
    tokio::time::timeout(tokio::time::Duration::from_secs(10), queued_rx.recv_async())
        .await
        .expect("Timed out waiting for the bundle to queue")
        .expect("Storage wrapper gone");

    // Arm the poll: it recovers the queued bundle and pushes it twice.
    arm_tx.send(()).expect("Storage wrapper gone");
    tokio::time::timeout(
        tokio::time::Duration::from_secs(10),
        injected_rx.recv_async(),
    )
    .await
    .expect("Timed out waiting for the duplicate injection")
    .expect("Storage wrapper gone");

    // The first copy wins the dequeue claim and is delivered (and held).
    tokio::time::timeout(
        tokio::time::Duration::from_secs(10),
        started_rx.recv_async(),
    )
    .await
    .expect("Timed out waiting for the delivery to start")
    .expect("Holding service gone");

    // Dispatch a marker bundle to the other service. It enters the dispatch
    // queue strictly after the injected duplicate (the injection completed
    // above), and the consumer handles the queue in order — so once the
    // marker's delivery starts, the duplicate has already been through the
    // dequeue claim, while the first delivery is verifiably still held.
    let (_, marker_data) = Builder::new("ipn:0.2.1".parse().unwrap(), "ipn:0.1.8".parse().unwrap())
        .with_lifetime(Duration::from_secs(3600))
        .with_payload(Cow::Borrowed(b"marker".as_slice()))
        .build(CreationTimestamp::now())
        .expect("Failed to build bundle");
    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(None, None, &mut Bytes::from(marker_data))
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );
    tokio::time::timeout(
        tokio::time::Duration::from_secs(10),
        marker_started_rx.recv_async(),
    )
    .await
    .expect("Timed out waiting for the marker delivery")
    .expect("Marker service gone");

    // Only now release the held delivery and drain the pipeline. shutdown()
    // is the barrier: it joins the dispatcher pools, so every spawned
    // delivery has completed (and been counted) by the time it returns — no
    // quiet window is involved.
    drop(release_tx);
    bpa.shutdown().await;

    assert_eq!(
        svc.deliveries.load(Ordering::SeqCst),
        1,
        "a stale poller copy must lose the dequeue claim, not redeliver"
    );
    assert_eq!(marker_svc.deliveries.load(Ordering::SeqCst), 1);
}
