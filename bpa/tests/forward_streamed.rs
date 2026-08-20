//! Integration tests for `Cla::forward_streamed` — the streamed egress door
//! and its buffered default adapter.

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
    /// `forward` was invoked with the whole bundle.
    Forward(Bytes),
    /// `forward_streamed` was invoked and pulled the stream to completion.
    Streamed {
        segments: Vec<Segment>,
        total_len: u64,
    },
    /// `forward_streamed` failed the attempt.
    Failed,
}

// ---------------------------------------------------------------------------
// Mock CLAs
// ---------------------------------------------------------------------------

/// Overrides `forward_streamed`, recording every segment it pulls.
struct StreamingCla {
    sink: hardy_async::sync::spin::Once<Box<dyn cla::Sink>>,
    events_tx: flume::Sender<Event>,
    /// When set, the first `forward_streamed` call fails without pulling.
    flaky: AtomicBool,
}

impl StreamingCla {
    fn new(flaky: bool) -> (Arc<Self>, flume::Receiver<Event>) {
        let (tx, rx) = flume::bounded(16);
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                events_tx: tx,
                flaky: AtomicBool::new(flaky),
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

    async fn forward(
        &self,
        _queue: Option<u32>,
        _cla_addr: &cla::ClaAddress,
        _bundle_id: &hardy_bpv7::bundle::Id,
        bundle: Bytes,
    ) -> cla::Result<cla::ForwardBundleResult> {
        // A call here means the streamed door was bypassed — observable,
        // so the test fails on the assertion rather than inside a BPA task.
        let _ = self.events_tx.send(Event::Forward(bundle));
        Ok(cla::ForwardBundleResult::Sent)
    }

    async fn forward_streamed(
        &self,
        _queue: Option<u32>,
        _cla_addr: &cla::ClaAddress,
        _bundle_id: &hardy_bpv7::bundle::Id,
        stream: &dyn Receiver<Segment>,
        total_len: u64,
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

/// Relies on the provided `forward_streamed` — the buffered default adapter.
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

    async fn forward(
        &self,
        _queue: Option<u32>,
        _cla_addr: &cla::ClaAddress,
        _bundle_id: &hardy_bpv7::bundle::Id,
        bundle: Bytes,
    ) -> cla::Result<cla::ForwardBundleResult> {
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
        _source: &Eid,
        _expiry: time::OffsetDateTime,
        _ack_requested: bool,
        _payload: Bytes,
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
// Full-path tests: dispatcher -> forward_streamed
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

    let parsed = hardy_bpv7::bundle::ParsedBundle::parse(data, hardy_bpv7::bpsec::no_keys)
        .expect("Failed to parse forwarded bundle");
    assert_eq!(parsed.bundle.id.source, source_eid);
    assert_eq!(parsed.bundle.destination, dest);

    assert!(events_rx.is_empty());
    bpa.shutdown().await;
}

/// A CLA that only implements `forward` still receives the whole bundle,
/// through the buffered default adapter.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn buffered_cla_receives_whole_bundle_via_default_adapter() {
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
        panic!("Expected the buffered adapter to call forward");
    };
    let parsed = hardy_bpv7::bundle::ParsedBundle::parse(&data, hardy_bpv7::bpsec::no_keys)
        .expect("Failed to parse forwarded bundle");
    assert_eq!(parsed.bundle.id.source, source_eid);
    assert_eq!(parsed.bundle.destination, dest);

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
// Direct-call tests: the default adapter body
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

/// The default body reassembles a multi-segment stream before delegating.
#[tokio::test]
async fn default_body_concats_multi_segment_stream() {
    let (cla, events_rx, bundle, data, addr) = direct_call_fixture();
    let (head, tail) = (data.slice(..10), data.slice(10..));
    let rx = feed(vec![Segment::Next(head), Segment::Final(tail)]).await;

    let result = cla
        .forward_streamed(None, &addr, &bundle.id, &rx, data.len() as u64)
        .await
        .unwrap();
    assert!(matches!(result, cla::ForwardBundleResult::Sent));

    let Ok(Event::Forward(received)) = events_rx.try_recv() else {
        panic!("Expected forward to receive the reassembled bundle");
    };
    assert_eq!(received, data);
}

/// A single-`Final` stream passes through the default body zero-copy.
#[tokio::test]
async fn default_body_is_zero_copy_for_single_final() {
    let (cla, events_rx, bundle, data, addr) = direct_call_fixture();
    let rx = feed(vec![Segment::Final(data.clone())]).await;

    cla.forward_streamed(None, &addr, &bundle.id, &rx, data.len() as u64)
        .await
        .unwrap();

    let Ok(Event::Forward(received)) = events_rx.try_recv() else {
        panic!("Expected forward to receive the bundle");
    };
    assert_eq!(received.as_ptr(), data.as_ptr());
}

/// A truncated stream is an error, and `forward` is never invoked — no
/// partial bundle reaches the transport.
#[tokio::test]
async fn default_body_truncated_stream_is_cancelled() {
    let (cla, events_rx, bundle, data, addr) = direct_call_fixture();
    let rx = feed(vec![Segment::Next(data.slice(..4))]).await;

    let Err(err) = cla
        .forward_streamed(None, &addr, &bundle.id, &rx, data.len() as u64)
        .await
    else {
        panic!("Expected a truncated stream to fail");
    };
    assert!(matches!(err, cla::Error::StreamCancelled));
    assert!(events_rx.is_empty());
}

/// A stream completing with fewer bytes than the declared `total_len` is
/// rejected before delegation — no short transfer reaches the transport.
#[tokio::test]
async fn default_body_rejects_under_delivering_stream() {
    let (cla, events_rx, bundle, data, addr) = direct_call_fixture();
    let short = data.slice(..data.len() - 1);
    let rx = feed(vec![Segment::Final(short.clone())]).await;

    let Err(err) = cla
        .forward_streamed(None, &addr, &bundle.id, &rx, data.len() as u64)
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

/// A stream exceeding the declared `total_len` is rejected before delegation.
#[tokio::test]
async fn default_body_rejects_stream_exceeding_total_len() {
    let (cla, events_rx, bundle, data, addr) = direct_call_fixture();
    let rx = feed(vec![Segment::Final(data.clone())]).await;

    let Err(err) = cla.forward_streamed(None, &addr, &bundle.id, &rx, 4).await else {
        panic!("Expected an oversize stream to fail");
    };
    assert!(matches!(
        err,
        cla::Error::PayloadTooLarge { size, max: 4 } if size > 4
    ));
    assert!(events_rx.is_empty());
}
