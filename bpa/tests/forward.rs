//! Integration tests for `Cla::forward` — the streamed egress door — and
//! `stream::buffer_stream`, the whole-buffer convenience used by CLAs that
//! need a contiguous bundle.

use core::num::NonZeroU64;
use std::sync::{Arc, Mutex};

use hardy_bpa::{
    Bytes, async_trait,
    bpa::{Bpa, BpaRegistration},
    cla::{self, Cla, ClaInit},
    services,
    stream::{Receiver, Segment},
};
use hardy_bpv7::eid::{Eid, IpnNodeId, NodeId};

// ---------------------------------------------------------------------------
// Events observed by the mock CLAs
// ---------------------------------------------------------------------------

enum Event {
    /// The buffering CLA assembled the whole bundle.
    Forward(Bytes),
    /// The streaming CLA pulled the stream to completion.
    Streamed {
        segments: Vec<Segment>,
        total_len: u64,
    },
    /// The forward attempt failed.
    Failed,
}

// ---------------------------------------------------------------------------
// Mock CLAs
// ---------------------------------------------------------------------------

/// Consumes the stream segment by segment, recording every segment it pulls.
struct StreamingCla {
    sink: hardy_async::sync::spin::Once<Box<dyn cla::Sink>>,
    events_tx: flume::Sender<Event>,
    /// When set, the first `forward` call fails with this error without
    /// pulling the stream.
    first_error: Mutex<Option<cla::Error>>,
}

impl StreamingCla {
    fn new(first_error: Option<cla::Error>) -> (Arc<Self>, flume::Receiver<Event>) {
        let (tx, rx) = flume::bounded(16);
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                events_tx: tx,
                first_error: Mutex::new(first_error),
            }),
            rx,
        )
    }
}

#[async_trait]
impl cla::Cla for StreamingCla {
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
        _bundle_id: &hardy_bpv7::bundle::Id,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> cla::Result<cla::ForwardBundleResult> {
        if let Some(e) = self.first_error.lock().unwrap().take() {
            let _ = self.events_tx.send(Event::Failed);
            return Err(e);
        }
        let mut segments = Vec::new();
        loop {
            match stream.recv().await {
                Ok(segment @ Segment::Next(_)) => segments.push(segment),
                Ok(segment @ Segment::Final(_)) => {
                    segments.push(segment);
                    break;
                }
                Err(_) => {
                    let _ = self.events_tx.send(Event::Failed);
                    return Err(cla::Error::StreamCancelled);
                }
            }
        }
        let _ = self.events_tx.send(Event::Streamed {
            segments,
            total_len,
        });
        Ok(cla::ForwardBundleResult::Sent)
    }
}

/// Buffers the stream into a contiguous bundle via `stream::buffer_stream`
/// — the shape every whole-buffer CLA takes on the streamed-only door.
struct BufferedCla {
    sink: hardy_async::sync::spin::Once<Box<dyn cla::Sink>>,
    events_tx: flume::Sender<Event>,
}

impl BufferedCla {
    fn new() -> (Arc<Self>, flume::Receiver<Event>) {
        let (tx, rx) = flume::bounded(16);
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                events_tx: tx,
            }),
            rx,
        )
    }
}

#[async_trait]
impl cla::Cla for BufferedCla {
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
        _bundle_id: &hardy_bpv7::bundle::Id,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> cla::Result<cla::ForwardBundleResult> {
        let bundle = hardy_bpa::stream::buffer_stream(stream, total_len).await?;
        let _ = self.events_tx.send(Event::Forward(bundle));
        Ok(cla::ForwardBundleResult::Sent)
    }
}

// ---------------------------------------------------------------------------
// Minimal application to originate bundles
// ---------------------------------------------------------------------------

struct SendOnlyApp {
    sink: hardy_async::sync::spin::Once<Box<dyn services::ApplicationSink>>,
}

impl SendOnlyApp {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            sink: hardy_async::sync::spin::Once::new(),
        })
    }
}

#[async_trait]
impl services::Application for SendOnlyApp {
    async fn on_register(&self, _source: &Eid, sink: Box<dyn services::ApplicationSink>) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn on_deliver(
        &self,
        _bundle_id: &hardy_bpv7::bundle::Id,
        _expiry: time::OffsetDateTime,
        _ack_requested: bool,
        _total_len: u64,
        _stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        Ok(())
    }

    async fn on_status_notify(
        &self,
        _bundle_id: &hardy_bpv7::bundle::Id,
        _from: &Eid,
        _kind: services::StatusNotify,
        _reason: hardy_bpv7::status_report::ReasonCode,
        _timestamp: Option<time::OffsetDateTime>,
    ) {
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_bundle(
    source: &Eid,
    destination: &Eid,
    payload: &[u8],
) -> (hardy_bpv7::bundle::Bundle, Bytes) {
    let (bundle, data) = hardy_bpv7::builder::Builder::new(source.clone(), destination.clone())
        .with_payload(std::borrow::Cow::Borrowed(payload))
        .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
        .expect("Failed to build bundle");
    (bundle, Bytes::from(data))
}

/// A pre-filled segment stream. The sender is dropped on return, so a
/// sequence not ending in `Final` reads as a truncated stream.
async fn feed(segments: Vec<Segment>) -> hardy_async::channel::Receiver<Segment> {
    let (tx, rx) = hardy_async::channel::bounded(segments.len().max(1));
    for segment in segments {
        hardy_async::channel::Sender::send(&tx, segment)
            .await
            .unwrap();
    }
    rx
}

fn remote_node(node_number: u32) -> NodeId {
    NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number,
    })
}

async fn recv_event(rx: &flume::Receiver<Event>, secs: u64) -> Event {
    tokio::time::timeout(tokio::time::Duration::from_secs(secs), rx.recv_async())
        .await
        .expect("Timed out waiting for CLA event")
        .expect("CLA event channel closed")
}

/// Sends `payload` to ipn:0.2.99 via `app`, returning the destination EID.
async fn originate(app: &SendOnlyApp, payload: &'static [u8]) -> Eid {
    let dest: Eid = "ipn:0.2.99".parse().unwrap();
    app.sink
        .get()
        .unwrap()
        .send(
            dest.clone(),
            Bytes::from_static(payload),
            core::time::Duration::from_secs(3600),
            None,
        )
        .await
        .unwrap();
    dest
}

// ---------------------------------------------------------------------------
// Full-path tests: dispatcher -> forward
// ---------------------------------------------------------------------------

/// A streaming CLA receives the whole bundle as a single `Final` segment
/// with an exact `total_len`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streaming_cla_receives_single_final_segment() {
    let bpa = Bpa::builder().build().await.unwrap();
    bpa.start(false).await;

    let (cla, events_rx) = StreamingCla::new(None);
    bpa.register_cla("stream".to_string(), cla.clone(), None, ClaInit::default())
        .await
        .unwrap();
    cla.sink
        .get()
        .unwrap()
        .add_peer(
            cla::ClaAddress::Private("peer".as_bytes().into()),
            &[remote_node(2)],
        )
        .await
        .unwrap();

    let app = SendOnlyApp::new();
    let source_eid = bpa
        .register_application(hardy_bpv7::eid::Service::Ipn(42), app.clone())
        .await
        .unwrap();
    let dest = originate(&app, b"Hello remote").await;

    let Event::Streamed {
        segments,
        total_len,
    } = recv_event(&events_rx, 5).await
    else {
        panic!("Expected the streamed door, got another event");
    };
    assert_eq!(segments.len(), 1);
    let Segment::Final(data) = &segments[0] else {
        panic!("Expected a single Final segment");
    };
    assert_eq!(total_len, data.len() as u64);

    let parsed = hardy_bpv7::parse::parse(data.clone()).expect("Failed to parse forwarded bundle");
    assert_eq!(parsed.bundle.primary.id.source, source_eid);
    assert_eq!(parsed.bundle.primary.destination, dest);

    assert!(events_rx.is_empty());
    bpa.shutdown().await;
}

/// A CLA that buffers via `stream::buffer_stream` still receives the whole
/// bundle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn buffered_cla_receives_whole_bundle() {
    let bpa = Bpa::builder().build().await.unwrap();
    bpa.start(false).await;

    let (cla, events_rx) = BufferedCla::new();
    bpa.register_cla(
        "buffered".to_string(),
        cla.clone(),
        None,
        ClaInit::default(),
    )
    .await
    .unwrap();
    cla.sink
        .get()
        .unwrap()
        .add_peer(
            cla::ClaAddress::Private("peer".as_bytes().into()),
            &[remote_node(2)],
        )
        .await
        .unwrap();

    let app = SendOnlyApp::new();
    let source_eid = bpa
        .register_application(hardy_bpv7::eid::Service::Ipn(42), app.clone())
        .await
        .unwrap();
    let dest = originate(&app, b"Hello adapter").await;

    let Event::Forward(data) = recv_event(&events_rx, 5).await else {
        panic!("Expected the buffering CLA to assemble the bundle");
    };
    let parsed = hardy_bpv7::parse::parse(data.clone()).expect("Failed to parse forwarded bundle");
    assert_eq!(parsed.bundle.primary.id.source, source_eid);
    assert_eq!(parsed.bundle.primary.destination, dest);

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// Direct-call tests: a buffering CLA over `stream::buffer_stream`
// ---------------------------------------------------------------------------

fn direct_call_fixture() -> (
    Arc<BufferedCla>,
    flume::Receiver<Event>,
    hardy_bpv7::bundle::Bundle,
    Bytes,
    cla::ClaAddress,
) {
    let (cla, events_rx) = BufferedCla::new();
    let (bundle, data) = build_bundle(
        &"ipn:0.1.1".parse().unwrap(),
        &"ipn:0.2.99".parse().unwrap(),
        b"payload",
    );
    let addr = cla::ClaAddress::Private("x".as_bytes().into());
    (cla, events_rx, bundle, data, addr)
}

/// The buffering path reassembles a multi-segment stream.
#[tokio::test]
async fn buffering_cla_concats_multi_segment_stream() {
    let (cla, events_rx, bundle, data, addr) = direct_call_fixture();
    let (head, tail) = (data.slice(..10), data.slice(10..));
    let mut rx = feed(vec![Segment::Next(head), Segment::Final(tail)]).await;

    let result = cla
        .forward(None, &addr, &bundle.primary.id, data.len() as u64, &mut rx)
        .await
        .unwrap();
    assert!(matches!(result, cla::ForwardBundleResult::Sent));

    let Ok(Event::Forward(received)) = events_rx.try_recv() else {
        panic!("Expected forward to receive the reassembled bundle");
    };
    assert_eq!(received, data);
}

/// A single-`Final` stream passes through the buffering path zero-copy.
#[tokio::test]
async fn buffering_cla_is_zero_copy_for_single_final() {
    let (cla, events_rx, bundle, data, addr) = direct_call_fixture();
    let mut rx = feed(vec![Segment::Final(data.clone())]).await;

    cla.forward(None, &addr, &bundle.primary.id, data.len() as u64, &mut rx)
        .await
        .unwrap();

    let Ok(Event::Forward(received)) = events_rx.try_recv() else {
        panic!("Expected forward to receive the bundle");
    };
    assert_eq!(received.as_ptr(), data.as_ptr());
}

/// A truncated stream is an error, and no partial bundle reaches the
/// transport.
#[tokio::test]
async fn buffering_cla_truncated_stream_is_cancelled() {
    let (cla, events_rx, bundle, data, addr) = direct_call_fixture();
    let mut rx = feed(vec![Segment::Next(data.slice(..4))]).await;

    let Err(err) = cla
        .forward(None, &addr, &bundle.primary.id, data.len() as u64, &mut rx)
        .await
    else {
        panic!("Expected a truncated stream to fail");
    };
    assert!(matches!(err, cla::Error::StreamCancelled));
    assert!(events_rx.is_empty());
}

/// A stream completing with fewer bytes than the declared `total_len` is
/// rejected — no short transfer reaches the transport.
#[tokio::test]
async fn buffering_cla_rejects_under_delivering_stream() {
    let (cla, events_rx, bundle, data, addr) = direct_call_fixture();
    let short = data.slice(..data.len() - 1);
    let mut rx = feed(vec![Segment::Final(short.clone())]).await;

    let Err(err) = cla
        .forward(None, &addr, &bundle.primary.id, data.len() as u64, &mut rx)
        .await
    else {
        panic!("Expected an under-delivering stream to fail");
    };
    assert!(matches!(
        err,
        cla::Error::PayloadUnderrun { size, expected }
            if size == short.len() as u64 && expected == data.len() as u64
    ));
    assert!(events_rx.is_empty());
}

/// A stream exceeding the declared `total_len` is rejected.
#[tokio::test]
async fn buffering_cla_rejects_stream_exceeding_total_len() {
    let (cla, events_rx, bundle, data, addr) = direct_call_fixture();
    let mut rx = feed(vec![Segment::Final(data.clone())]).await;

    let Err(err) = cla
        .forward(None, &addr, &bundle.primary.id, 4, &mut rx)
        .await
    else {
        panic!("Expected an oversize stream to fail");
    };
    assert!(matches!(
        err,
        cla::Error::PayloadTooLarge { size, max: 4 } if size > 4
    ));
    assert!(events_rx.is_empty());
}

// ---------------------------------------------------------------------------
// Forwarding-outcome behaviour at the streamed egress door: a synchronous
// `Cla::forward` error is retried by its kind. Grouped in a module so the
// storage rig's helpers do not collide with the door and buffer_stream
// tests above; the mock CLA is the file-level `StreamingCla`.
// ---------------------------------------------------------------------------
mod outcomes {
    use std::sync::Arc;

    use hardy_bpa::{
        Bytes, async_trait,
        bpa::{Bpa, BpaRegistration},
        bundle::{Bundle, BundleStatus},
        cla::{self, ClaInit},
        node_ids::NodeIds,
        storage::{ConfirmResponse, MetadataMemStorage, MetadataStorage, Result as StorageResult},
        stream::{Segment, Sender},
    };
    use hardy_bpv7::{
        bundle::Id,
        eid::{Eid, IpnNodeId, NodeId},
    };

    use super::{Event, StreamingCla, remote_node};

    /// Builds a bundle from ipn:0.3.1 to ipn:0.2.99; the creation timestamp
    /// makes each id unique.
    fn test_bundle(payload: &[u8]) -> (Id, Bytes) {
        let (bundle, data) = hardy_bpv7::builder::Builder::new(
            "ipn:0.3.1".parse().unwrap(),
            "ipn:0.2.99".parse().unwrap(),
        )
        .with_payload(std::borrow::Cow::Borrowed(payload))
        .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
        .expect("Failed to build bundle");
        (bundle.primary.id, Bytes::from(data))
    }

    async fn recv_event(rx: &flume::Receiver<Event>) -> Event {
        super::recv_event(rx, 5).await
    }

    // A metadata-store decorator that nudges a channel after every write
    // (point transitions and bulk resets alike), so a [`StatusWatcher`]
    // re-reads the store's true status on the change itself rather than
    // polling on a timer. All calls delegate to the shared in-memory
    // store, which the watcher reads directly.
    struct SignalingMem {
        store: Arc<MetadataMemStorage>,
        tx: flume::Sender<()>,
    }

    #[async_trait]
    impl MetadataStorage for SignalingMem {
        async fn get(&self, bundle_id: &Id) -> StorageResult<Option<Bundle>> {
            self.store.get(bundle_id).await
        }

        async fn insert(&self, bundle: &Bundle) -> StorageResult<bool> {
            self.store.insert(bundle).await
        }

        async fn replace(&self, bundle: &Bundle) -> StorageResult<()> {
            self.store.replace(bundle).await
        }

        async fn swap_status(
            &self,
            bundle_id: &Id,
            expected: &BundleStatus,
            status: &BundleStatus,
        ) -> StorageResult<bool> {
            let swapped = self.store.swap_status(bundle_id, expected, status).await;
            let _ = self.tx.send(());
            swapped
        }

        async fn tombstone_if(
            &self,
            bundle_id: &Id,
            expected: &BundleStatus,
        ) -> StorageResult<bool> {
            let tombstoned = self.store.tombstone_if(bundle_id, expected).await;
            let _ = self.tx.send(());
            tombstoned
        }

        async fn tombstone(&self, bundle_id: &Id) -> StorageResult<()> {
            let result = self.store.tombstone(bundle_id).await;
            let _ = self.tx.send(());
            result
        }

        async fn start_recovery(&self) {
            self.store.start_recovery().await
        }

        async fn confirm_exists(&self, bundle_id: &Id) -> StorageResult<Option<ConfirmResponse>> {
            self.store.confirm_exists(bundle_id).await
        }

        async fn remove_unconfirmed(&self, stream: &dyn Sender<Bundle>) -> StorageResult<()> {
            self.store.remove_unconfirmed(stream).await
        }

        async fn reset_peer_queue(&self, peer: u32) -> StorageResult<u64> {
            // A bulk transition: nudge so the watcher re-reads each bundle
            // this reset to Waiting.
            let reset = self.store.reset_peer_queue(peer).await;
            let _ = self.tx.send(());
            reset
        }

        async fn reset_peer_ack_pending(&self, peer: u32) -> StorageResult<u64> {
            let reset = self.store.reset_peer_ack_pending(peer).await;
            let _ = self.tx.send(());
            reset
        }

        async fn reset_service_queue(&self, service: &Eid) -> StorageResult<u64> {
            let reset = self.store.reset_service_queue(service).await;
            let _ = self.tx.send(());
            reset
        }

        async fn poll_expiry(&self, stream: &dyn Sender<Bundle>) -> StorageResult<()> {
            self.store.poll_expiry(stream).await
        }

        async fn poll_waiting(&self, stream: &dyn Sender<Bundle>) -> StorageResult<()> {
            self.store.poll_waiting(stream).await
        }

        async fn poll_service_waiting(
            &self,
            source: Eid,
            stream: &dyn Sender<Bundle>,
        ) -> StorageResult<()> {
            self.store.poll_service_waiting(source, stream).await
        }

        async fn poll_adu_fragments(
            &self,
            stream: &dyn Sender<Bundle>,
            status: &BundleStatus,
        ) -> StorageResult<()> {
            self.store.poll_adu_fragments(stream, status).await
        }

        async fn poll_pending(
            &self,
            stream: &dyn Sender<Bundle>,
            status: &BundleStatus,
            limit: usize,
        ) -> StorageResult<()> {
            self.store.poll_pending(stream, status, limit).await
        }
    }

    /// Reads a bundle's status directly from the store, waking on each
    /// [`SignalingMem`] nudge rather than polling on a timer. Reading the
    /// live store (not a cached signal value) means a bulk reset is
    /// observed as faithfully as a point write.
    struct StatusWatcher {
        store: Arc<MetadataMemStorage>,
        rx: flume::Receiver<()>,
    }

    impl StatusWatcher {
        // Waits until `id`'s status satisfies `accept` (`None` once
        // deleted). The timeout only bounds a regression.
        async fn wait(
            &mut self,
            id: &Id,
            what: &str,
            accept: impl Fn(Option<&BundleStatus>) -> bool,
        ) {
            loop {
                let bundle = self.store.get(id).await.unwrap();
                if accept(bundle.as_ref().map(|b| &b.status)) {
                    return;
                }
                tokio::time::timeout(tokio::time::Duration::from_secs(5), self.rx.recv_async())
                    .await
                    .unwrap_or_else(|_| panic!("Timed out waiting for {what}"))
                    .expect("nudge channel closed");
            }
        }
    }

    /// A started BPA (node ipn:0.1) with `cla` registered, and a
    /// [`StatusWatcher`] over its metadata store for status assertions.
    /// Callers add peers through the CLA's sink.
    async fn egress_setup(cla: Arc<dyn cla::Cla>) -> (Bpa, StatusWatcher) {
        let (tx, rx) = flume::unbounded();
        let store = Arc::new(MetadataMemStorage::new(None));
        let metadata_store = Arc::new(SignalingMem {
            store: store.clone(),
            tx,
        });
        let node_ids = NodeIds::try_from(
            [NodeId::Ipn(IpnNodeId {
                allocator_id: 0,
                node_number: 1,
            })]
            .as_slice(),
        )
        .unwrap();

        let bpa = Bpa::builder()
            .node_ids(node_ids)
            .metadata_storage(metadata_store)
            .build()
            .await
            .unwrap();
        bpa.start(false).await;

        bpa.register_cla("egress".to_string(), cla, None, ClaInit::default())
            .await
            .unwrap();

        (bpa, StatusWatcher { store, rx })
    }

    /// An interrupted streamed transfer (`StreamCancelled`) is transient:
    /// the bundle is re-dispatched over the same route straight away, with
    /// no routing event needed, and the retry streams the whole bundle.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn interrupted_streamed_forward_is_redispatched_and_retried() {
        let (cla, events_rx) = StreamingCla::new(Some(cla::Error::StreamCancelled));
        let (bpa, mut watcher) = egress_setup(cla.clone()).await;
        cla.sink
            .get()
            .unwrap()
            .add_peer(
                cla::ClaAddress::Private("peer-a".as_bytes().into()),
                &[remote_node(2)],
            )
            .await
            .unwrap();

        let (id, mut data) = test_bundle(b"try again streamed");
        cla.sink
            .get()
            .unwrap()
            .dispatch(None, None, &mut data)
            .await
            .unwrap();

        // The first attempt fails as an interrupted transfer.
        assert!(matches!(recv_event(&events_rx).await, Event::Failed));

        // An interrupted transfer re-dispatches over the same route
        // immediately: no fresh peer, and the retry streams the bundle.
        let Event::Streamed { segments, .. } = recv_event(&events_rx).await else {
            panic!("Expected a successful retry through the streamed door");
        };
        let Some(Segment::Final(forwarded)) = segments.last() else {
            panic!("Expected the retry to end on a Final segment");
        };
        let parsed = hardy_bpv7::parse::parse(forwarded.clone())
            .expect("Failed to parse the retried bundle");
        assert_eq!(
            parsed.bundle.primary.id, id,
            "The retry must be the same bundle"
        );

        // `Sent` resolves the retry terminally: exactly one retry.
        watcher
            .wait(&id, "the retried bundle to be deleted", |st| st.is_none())
            .await;
        assert!(events_rx.is_empty(), "Exactly one retry is expected");

        bpa.shutdown().await;
    }

    /// A synchronous rejection from the CLA is bundle-scoped evidence: the
    /// bundle parks in `Waiting` (an unchanged routing decision is not
    /// re-run at pipeline speed), and the next routing event re-offers it.
    /// The successful retry resolves it terminally rather than looping.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn synchronous_forward_error_parks_bundle_until_routing_event() {
        let (cla, events_rx) = StreamingCla::new(Some(cla::Error::Internal(
            "the CLA rejects this bundle".into(),
        )));
        let (bpa, mut watcher) = egress_setup(cla.clone()).await;
        cla.sink
            .get()
            .unwrap()
            .add_peer(
                cla::ClaAddress::Private("peer-a".as_bytes().into()),
                &[remote_node(2)],
            )
            .await
            .unwrap();

        let (id, mut data) = test_bundle(b"try again");
        cla.sink
            .get()
            .unwrap()
            .dispatch(None, None, &mut data)
            .await
            .unwrap();

        // The first offer consumes the scripted rejection.
        assert!(matches!(recv_event(&events_rx).await, Event::Failed));

        // The rejection parks the bundle in Waiting. Parked means parked:
        // the status was reached without a second offer in between.
        watcher
            .wait(&id, "the rejected bundle to park in Waiting", |st| {
                matches!(st, Some(BundleStatus::Waiting))
            })
            .await;
        assert!(
            events_rx.is_empty(),
            "no re-offer may precede a routing event"
        );

        // A fresh peer for the same node re-dispatches the parked bundle;
        // the script is exhausted, so the retry streams and succeeds.
        cla.sink
            .get()
            .unwrap()
            .add_peer(
                cla::ClaAddress::Private("peer-b".as_bytes().into()),
                &[remote_node(2)],
            )
            .await
            .unwrap();
        let Event::Streamed { segments, .. } = recv_event(&events_rx).await else {
            panic!("Expected the re-dispatched offer to stream");
        };
        let Some(Segment::Final(forwarded)) = segments.last() else {
            panic!("Expected the retry to end on a Final segment");
        };
        let parsed = hardy_bpv7::parse::parse(forwarded.clone())
            .expect("Failed to parse the retried bundle");
        assert_eq!(
            parsed.bundle.primary.id, id,
            "Re-offer must be the same bundle"
        );

        watcher
            .wait(&id, "the retried bundle to be deleted", |st| st.is_none())
            .await;
        // The bundle is resolved; no further offer follows.
        assert!(events_rx.is_empty(), "no further offer after resolution");

        bpa.shutdown().await;
    }
}
