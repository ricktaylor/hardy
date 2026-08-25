//! Integration tests for `Cla::forward` — the streamed egress door — and
//! `stream::buffer_stream`, the whole-buffer convenience used by CLAs that
//! need a contiguous bundle.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use hardy_bpa::{
    Bytes, async_trait,
    bpa::{Bpa, BpaRegistration},
    cla::{self, Cla},
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
    /// When set, the first `forward` call fails without pulling.
    flaky: AtomicBool,
    /// The explicit egress lanes this CLA declares.
    lanes: Option<core::num::NonZeroU32>,
}

impl StreamingCla {
    fn new(flaky: bool) -> (Arc<Self>, flume::Receiver<Event>) {
        Self::with_lanes(flaky, None)
    }

    fn with_lanes(
        flaky: bool,
        lanes: Option<core::num::NonZeroU32>,
    ) -> (Arc<Self>, flume::Receiver<Event>) {
        let (tx, rx) = flume::bounded(16);
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                events_tx: tx,
                flaky: AtomicBool::new(flaky),
                lanes,
            }),
            rx,
        )
    }
}

#[async_trait]
impl cla::Cla for StreamingCla {
    async fn on_register(&self, sink: Box<dyn cla::Sink>, _node_ids: &[NodeId]) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    fn lane_count(&self) -> Option<core::num::NonZeroU32> {
        self.lanes
    }

    async fn forward(
        &self,
        _lane: Option<u32>,
        _cla_addr: &cla::ClaAddress,
        _bundle_id: &hardy_bpv7::bundle::Id,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> cla::Result<cla::ForwardBundleResult> {
        if self.flaky.swap(false, Ordering::SeqCst) {
            let _ = self.events_tx.send(Event::Failed);
            return Err(cla::Error::StreamCancelled);
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
    async fn on_register(&self, sink: Box<dyn cla::Sink>, _node_ids: &[NodeId]) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    fn lane_count(&self) -> Option<core::num::NonZeroU32> {
        None
    }

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
        _adu_size: u64,
        _stream: &mut dyn hardy_bpa::stream::Receiver<Segment>,
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
            core::time::Duration::from_secs(3600),
            None,
            &mut Bytes::from_static(payload),
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
    bpa.start(false);

    let (cla, events_rx) = StreamingCla::new(false);
    bpa.register_cla("stream".to_string(), cla.clone(), None)
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
    bpa.start(false);

    let (cla, events_rx) = BufferedCla::new();
    bpa.register_cla("buffered".to_string(), cla.clone(), None)
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

/// A failed streamed attempt takes the established requeue path: the bundle
/// returns to Waiting and a routing change re-dispatches it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_streamed_forward_is_requeued_and_retried() {
    let bpa = Bpa::builder().build().await.unwrap();
    bpa.start(false);

    let (cla, events_rx) = StreamingCla::new(true);
    bpa.register_cla("flaky".to_string(), cla.clone(), None)
        .await
        .unwrap();
    cla.sink
        .get()
        .unwrap()
        .add_peer(
            cla::ClaAddress::Private("peer-a".as_bytes().into()),
            &[remote_node(2)],
        )
        .await
        .unwrap();

    let app = SendOnlyApp::new();
    bpa.register_application(hardy_bpv7::eid::Service::Ipn(42), app.clone())
        .await
        .unwrap();
    originate(&app, b"Try again").await;

    assert!(matches!(recv_event(&events_rx, 5).await, Event::Failed));

    // Nudge the RIB so the Waiting bundle is re-polled. The failed attempt
    // returns the bundle to Waiting *after* the CLA reports the failure, so
    // a single nudge can race it; each fresh peer re-triggers the poll.
    let mut retry = None;
    for i in 0.. {
        cla.sink
            .get()
            .unwrap()
            .add_peer(
                cla::ClaAddress::Private(format!("peer-{i}").into_bytes().into()),
                &[remote_node(2)],
            )
            .await
            .unwrap();
        if let Ok(Ok(event)) = tokio::time::timeout(
            tokio::time::Duration::from_millis(500),
            events_rx.recv_async(),
        )
        .await
        {
            retry = Some(event);
            break;
        }
        assert!(i < 20, "Timed out waiting for the retry");
    }
    let Some(Event::Streamed { segments, .. }) = retry else {
        panic!("Expected a successful retry through the streamed door");
    };
    assert!(matches!(segments.last(), Some(Segment::Final(_))));

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
            if size == short.len() && expected == data.len()
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

/// A CLA declaring explicit egress lanes registers, adds peers, and
/// forwards under the default (null) egress policy: the declaration
/// is tolerated and ignored, never a panic, and an over-declaration is
/// clamped rather than allocated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn declared_lanes_are_tolerated_and_clamped() {
    let bpa = Bpa::builder().build().await.unwrap();
    bpa.start(false);

    let (cla, events_rx) = StreamingCla::with_lanes(false, core::num::NonZeroU32::new(u32::MAX));
    bpa.register_cla("laned".to_string(), cla.clone(), None)
        .await
        .unwrap();

    // An over-declared lane count must not drive a per-lane
    // allocation loop: this add_peer completing at all is the test.
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
    bpa.register_application(hardy_bpv7::eid::Service::Ipn(42), app.clone())
        .await
        .unwrap();
    originate(&app, b"through the lanes").await;

    let Event::Streamed { segments, .. } = recv_event(&events_rx, 5).await else {
        panic!("Expected the streamed door");
    };
    assert_eq!(segments.len(), 1);

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// Dispatcher forwarding-outcome behaviour at the streamed egress door:
// success, bundle-scoped failure, and link-scoped failure. Grouped in a
// module so its mock CLAs and helpers do not collide with the door and
// buffer_stream tests above.
// ---------------------------------------------------------------------------
mod outcomes {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use hardy_bpa::{
        Bytes, async_trait,
        bpa::{Bpa, BpaRegistration},
        bundle::{Bundle, BundleMetadata, BundleStatus},
        cla,
        storage::{MetadataMemStorage, MetadataStorage, Result as StorageResult},
        stream::{Receiver, Segment, Sender},
    };
    use hardy_bpv7::{
        bundle::Id,
        eid::{Eid, IpnNodeId, NodeId},
    };

    // ---------------------------------------------------------------------------
    // Events observed by the mock CLAs
    // ---------------------------------------------------------------------------

    enum Event {
        /// A buffering `forward` assembled the whole bundle.
        Forward { id: Id },
        /// A streaming `forward` pulled the stream to completion.
        Streamed {
            id: Id,
            segments: Vec<Segment>,
            total_len: u64,
        },
    }

    // ---------------------------------------------------------------------------
    // Mock CLAs
    // ---------------------------------------------------------------------------

    /// A scripted response to one `forward` call.
    enum Scripted {
        /// Answer immediately.
        Reply(cla::Result<cla::ForwardBundleResult>),
        /// Hold the call until the paired sender fires, then answer.
        ReplyWhenReleased(flume::Receiver<()>, cla::Result<cla::ForwardBundleResult>),
    }

    /// Buffers the stream via `stream::buffer_stream` before recording it.
    ///
    /// Every `forward` call is recorded, then answered from the front of
    /// `script`, or with `Sent` once the script is exhausted.
    struct BufferedCla {
        sink: hardy_async::sync::spin::Once<Box<dyn cla::Sink>>,
        events_tx: flume::Sender<Event>,
        script: Mutex<VecDeque<Scripted>>,
    }

    impl BufferedCla {
        fn new(script: Vec<Scripted>) -> (Arc<Self>, flume::Receiver<Event>) {
            let (tx, rx) = flume::bounded(16);
            (
                Arc::new(Self {
                    sink: hardy_async::sync::spin::Once::new(),
                    events_tx: tx,
                    script: Mutex::new(script.into()),
                }),
                rx,
            )
        }

        fn sink(&self) -> &dyn cla::Sink {
            self.sink.get().unwrap().as_ref()
        }
    }

    #[async_trait]
    impl cla::Cla for BufferedCla {
        async fn on_register(&self, sink: Box<dyn cla::Sink>, _node_ids: &[NodeId]) {
            self.sink.call_once(|| sink);
        }

        async fn on_unregister(&self) {}

        fn lane_count(&self) -> Option<core::num::NonZeroU32> {
            None
        }

        async fn forward(
            &self,
            _lane: Option<u32>,
            _cla_addr: &cla::ClaAddress,
            bundle_id: &Id,
            total_len: u64,
            stream: &mut dyn Receiver<Segment>,
        ) -> cla::Result<cla::ForwardBundleResult> {
            // Buffering enforces full, exact consumption of the stream.
            hardy_bpa::stream::buffer_stream(stream, total_len).await?;
            let _ = self.events_tx.send(Event::Forward {
                id: bundle_id.clone(),
            });
            // The guard must not be held across the await below.
            let scripted = self.script.lock().unwrap().pop_front();
            match scripted {
                None => Ok(cla::ForwardBundleResult::Sent),
                Some(Scripted::Reply(reply)) => reply,
                Some(Scripted::ReplyWhenReleased(release, reply)) => {
                    let _ = release.recv_async().await;
                    reply
                }
            }
        }
    }

    /// Pulls the stream to completion, recording every segment.
    struct StreamingCla {
        sink: hardy_async::sync::spin::Once<Box<dyn cla::Sink>>,
        events_tx: flume::Sender<Event>,
    }

    impl StreamingCla {
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

        fn sink(&self) -> &dyn cla::Sink {
            self.sink.get().unwrap().as_ref()
        }
    }

    #[async_trait]
    impl cla::Cla for StreamingCla {
        async fn on_register(&self, sink: Box<dyn cla::Sink>, _node_ids: &[NodeId]) {
            self.sink.call_once(|| sink);
        }

        async fn on_unregister(&self) {}

        fn lane_count(&self) -> Option<core::num::NonZeroU32> {
            None
        }

        async fn forward(
            &self,
            _lane: Option<u32>,
            _cla_addr: &cla::ClaAddress,
            bundle_id: &Id,
            total_len: u64,
            stream: &mut dyn Receiver<Segment>,
        ) -> cla::Result<cla::ForwardBundleResult> {
            let mut segments = Vec::new();
            loop {
                match stream.recv().await {
                    Ok(segment @ Segment::Next(_)) => segments.push(segment),
                    Ok(segment @ Segment::Final(_)) => {
                        segments.push(segment);
                        break;
                    }
                    Err(_) => return Err(cla::Error::StreamCancelled),
                }
            }
            let _ = self.events_tx.send(Event::Streamed {
                id: bundle_id.clone(),
                segments,
                total_len,
            });
            Ok(cla::ForwardBundleResult::Sent)
        }
    }

    // ---------------------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------------------

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

    fn node_two() -> NodeId {
        NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 2,
        })
    }

    fn private_addr(tag: &'static str) -> cla::ClaAddress {
        cla::ClaAddress::Private(tag.as_bytes().into())
    }

    async fn recv_event(rx: &flume::Receiver<Event>) -> Event {
        tokio::time::timeout(tokio::time::Duration::from_secs(5), rx.recv_async())
            .await
            .expect("Timed out waiting for a CLA event")
            .expect("CLA event channel closed")
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

        async fn update_status(&self, bundle: &Bundle) -> StorageResult<()> {
            let result = self.store.update_status(bundle).await;
            let _ = self.tx.send(());
            result
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

        async fn confirm_exists(&self, bundle_id: &Id) -> StorageResult<Option<BundleMetadata>> {
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

        async fn poll_expiry(
            &self,
            stream: &dyn Sender<Bundle>,
            limit: usize,
        ) -> StorageResult<()> {
            self.store.poll_expiry(stream, limit).await
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
                if accept(bundle.as_ref().map(|b| &b.metadata.status)) {
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
        let node_ids = hardy_bpa::node_ids::NodeIds::try_from(
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
        bpa.start(false);

        bpa.register_cla("egress".to_string(), cla, None)
            .await
            .unwrap();

        (bpa, StatusWatcher { store, rx })
    }

    // ---------------------------------------------------------------------------
    // Full-path tests: dispatcher -> forward_streamed
    // ---------------------------------------------------------------------------

    /// A bundle entering through the CLA sink's dispatch door leaves through
    /// `forward` as a single `Final` segment with an exact `total_len`, and a
    /// `Sent` result resolves it terminally.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatcher_forwards_through_streamed_door() {
        let (cla, events_rx) = StreamingCla::new();
        let (bpa, mut watcher) = egress_setup(cla.clone()).await;
        cla.sink()
            .add_peer(private_addr("peer-a"), &[node_two()])
            .await
            .unwrap();

        let (_, mut data) = test_bundle(b"streamed egress");
        cla.sink().dispatch(None, None, &mut data).await.unwrap();

        let Event::Streamed {
            id,
            segments,
            total_len,
        } = recv_event(&events_rx).await
        else {
            panic!("Expected the streamed door");
        };
        let [Segment::Final(forwarded)] = segments.as_slice() else {
            panic!("Expected a single Final segment");
        };
        assert_eq!(total_len, forwarded.len() as u64);

        let parsed =
            hardy_bpv7::parse::parse(forwarded.clone()).expect("Failed to parse forwarded bundle");
        assert_eq!(parsed.bundle.primary.id, id);
        assert_eq!(
            parsed.bundle.primary.destination,
            "ipn:0.2.99".parse::<Eid>().unwrap()
        );

        // `Sent` resolves the transfer terminally.
        watcher
            .wait(&id, "the sent bundle to be deleted", |st| st.is_none())
            .await;
        assert!(events_rx.is_empty(), "Exactly one forward is expected");

        bpa.shutdown().await;
    }

    /// A synchronous `Err` from the CLA is bundle-scoped evidence: the bundle
    /// re-enters dispatch at once, and with the route unchanged it is re-offered
    /// to the same CLA without any routing nudge in between. The successful
    /// retry resolves it terminally rather than looping.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn synchronous_forward_error_redispatches_immediately() {
        let (cla, events_rx) = BufferedCla::new(vec![Scripted::Reply(Err(cla::Error::Internal(
            "transient transport fault".into(),
        )))]);
        let (bpa, mut watcher) = egress_setup(cla.clone()).await;
        cla.sink()
            .add_peer(private_addr("peer-a"), &[node_two()])
            .await
            .unwrap();

        let (id, mut data) = test_bundle(b"try again");
        cla.sink().dispatch(None, None, &mut data).await.unwrap();

        // The first offer consumes the scripted Err.
        let Event::Forward { id: first, .. } = recv_event(&events_rx).await else {
            panic!("Expected the first offer");
        };
        assert_eq!(first, id);

        // The re-dispatched offer arrives with no route change in between.
        let Event::Forward { id: retried, .. } = recv_event(&events_rx).await else {
            panic!("Expected the re-dispatched offer");
        };
        assert_eq!(retried, id, "Re-offer must be the same bundle");

        watcher
            .wait(&id, "the retried bundle to be deleted", |st| st.is_none())
            .await;
        // The bundle is resolved; no further offer follows.
        assert!(events_rx.is_empty(), "no further offer after resolution");

        bpa.shutdown().await;
    }

    /// `NoNeighbour` is link-scoped evidence: the offered bundle parks in
    /// `Waiting` with no immediate re-dispatch, and the peer's queue is reset so
    /// a bundle still queued behind it returns to `Waiting` without ever being
    /// offered. A fresh route event re-dispatches both.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_neighbour_parks_bundle_and_resets_peer_queue() {
        let (release_tx, release_rx) = flume::bounded(1);
        let (cla, events_rx) = BufferedCla::new(vec![Scripted::ReplyWhenReleased(
            release_rx,
            Ok(cla::ForwardBundleResult::NoNeighbour),
        )]);
        let (bpa, mut watcher) = egress_setup(cla.clone()).await;
        cla.sink()
            .add_peer(private_addr("peer-a"), &[node_two()])
            .await
            .unwrap();

        // Bundle A is offered and held inside the CLA, keeping the peer's queue
        // poller busy so bundle B stays queued behind it.
        let (id_a, mut data_a) = test_bundle(b"no-neighbour");
        cla.sink().dispatch(None, None, &mut data_a).await.unwrap();
        let Event::Forward { id: offered, .. } = recv_event(&events_rx).await else {
            panic!("Expected bundle A to be offered");
        };
        assert_eq!(offered, id_a);

        let (id_b, mut data_b) = test_bundle(b"queued behind");
        cla.sink().dispatch(None, None, &mut data_b).await.unwrap();
        watcher
            .wait(&id_b, "bundle B to queue for the peer", |st| {
                matches!(st, Some(BundleStatus::ForwardPending { .. }))
            })
            .await;

        // Release the held offer: the CLA answers NoNeighbour.
        release_tx.send(()).unwrap();

        // A parks in Waiting, and the peer queue reset returns B to Waiting too.
        watcher
            .wait(&id_a, "bundle A to park in Waiting", |st| {
                matches!(st, Some(BundleStatus::Waiting))
            })
            .await;
        watcher
            .wait(&id_b, "bundle B to return to Waiting", |st| {
                matches!(st, Some(BundleStatus::Waiting))
            })
            .await;

        // Parked means parked: with both bundles confirmed in Waiting,
        // neither reached the CLA before a route event, and B's stale
        // queue entry never surfaced as an offer.
        assert!(
            events_rx.is_empty(),
            "no bundle may reach the CLA while parked"
        );

        // A fresh peer for the same node re-dispatches both bundles; the script
        // is exhausted, so both offers are answered Sent.
        cla.sink()
            .add_peer(private_addr("peer-b"), &[node_two()])
            .await
            .unwrap();
        let mut offered = Vec::new();
        for _ in 0..2 {
            let Event::Forward { id, .. } = recv_event(&events_rx).await else {
                panic!("Expected a re-dispatched offer");
            };
            offered.push(id);
        }
        assert!(offered.contains(&id_a), "bundle A must be re-offered");
        assert!(offered.contains(&id_b), "bundle B must be re-offered");

        watcher
            .wait(&id_a, "bundle A to be deleted", |st| st.is_none())
            .await;
        watcher
            .wait(&id_b, "bundle B to be deleted", |st| st.is_none())
            .await;

        bpa.shutdown().await;
    }
}
