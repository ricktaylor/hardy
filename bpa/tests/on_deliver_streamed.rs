//! Integration tests for `Service::on_deliver_streamed` — the streamed
//! delivery door and its buffered default adapter.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use hardy_bpa::{
    Bytes, async_trait,
    bpa::{Bpa, BpaRegistration},
    cla, services,
    stream::{Receiver, Segment},
};
use hardy_bpv7::eid::{Eid, IpnNodeId, NodeId};

// ---------------------------------------------------------------------------
// Events observed by the mock services
// ---------------------------------------------------------------------------

enum Event {
    /// `on_deliver` was invoked with the whole bundle.
    Received(Bytes),
    /// `on_deliver_streamed` was invoked and pulled the stream to completion.
    Streamed {
        segments: Vec<Segment>,
        total_len: u64,
    },
    /// `on_deliver_streamed` failed the delivery.
    Failed,
}

// ---------------------------------------------------------------------------
// Mock services
// ---------------------------------------------------------------------------

/// Overrides `on_deliver_streamed`, recording every segment it pulls.
struct StreamingService {
    sink: hardy_async::sync::spin::Once<Box<dyn services::ServiceSink>>,
    events_tx: flume::Sender<Event>,
    /// When set, every `on_deliver_streamed` call fails without pulling.
    failing: AtomicBool,
}

impl StreamingService {
    fn new(failing: bool) -> (Arc<Self>, flume::Receiver<Event>) {
        let (tx, rx) = flume::bounded(16);
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                events_tx: tx,
                failing: AtomicBool::new(failing),
            }),
            rx,
        )
    }
}

#[async_trait]
impl services::Service for StreamingService {
    async fn on_register(&self, _endpoint: &Eid, sink: Box<dyn services::ServiceSink>) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn on_deliver(
        &self,
        _bundle_id: &hardy_bpv7::bundle::Id,
        data: Bytes,
        _expiry: time::OffsetDateTime,
    ) -> services::Result<()> {
        // A call here means the streamed door was bypassed — observable,
        // so the test fails on the assertion rather than inside a BPA task.
        let _ = self.events_tx.send(Event::Received(data));
        Ok(())
    }

    async fn on_deliver_streamed(
        &self,
        _bundle_id: &hardy_bpv7::bundle::Id,
        stream: &dyn Receiver<Segment>,
        _expiry: time::OffsetDateTime,
        total_len: u64,
    ) -> services::Result<()> {
        if self.failing.load(Ordering::SeqCst) {
            let _ = self.events_tx.send(Event::Failed);
            return Err(services::Error::StreamCancelled);
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
                    return Err(services::Error::StreamCancelled);
                }
            }
        }
        let _ = self.events_tx.send(Event::Streamed {
            segments,
            total_len,
        });
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

/// Relies on the provided `on_deliver_streamed` — the buffered default
/// adapter.
struct BufferedService {
    sink: hardy_async::sync::spin::Once<Box<dyn services::ServiceSink>>,
    events_tx: flume::Sender<Event>,
}

impl BufferedService {
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
impl services::Service for BufferedService {
    async fn on_register(&self, _endpoint: &Eid, sink: Box<dyn services::ServiceSink>) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn on_deliver(
        &self,
        _bundle_id: &hardy_bpv7::bundle::Id,
        data: Bytes,
        _expiry: time::OffsetDateTime,
    ) -> services::Result<()> {
        let _ = self.events_tx.send(Event::Received(data));
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
// Minimal CLA to inject inbound bundles
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
    async fn on_register(&self, sink: Box<dyn cla::Sink>, _node_ids: &[NodeId]) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn forward(
        &self,
        _queue: Option<u32>,
        _cla_addr: &cla::ClaAddress,
        _bundle_id: &hardy_bpv7::bundle::Id,
        _bundle: Bytes,
    ) -> cla::Result<cla::ForwardBundleResult> {
        Ok(cla::ForwardBundleResult::Sent)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_bundle(source: &Eid, destination: &Eid, payload: &[u8]) -> Bytes {
    let (_, data) = hardy_bpv7::builder::Builder::new(source.clone(), destination.clone())
        .with_payload(std::borrow::Cow::Borrowed(payload))
        .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
        .expect("Failed to build bundle");
    Bytes::from(data)
}

/// The identity of a bundle built by [`build_bundle`], for direct
/// `on_deliver_streamed` calls.
fn bundle_id_of(data: &Bytes) -> hardy_bpv7::bundle::Id {
    hardy_bpv7::bundle::ParsedBundle::parse(data, hardy_bpv7::bpsec::no_keys)
        .expect("Failed to parse built bundle")
        .bundle
        .id
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

async fn recv_event(rx: &flume::Receiver<Event>, secs: u64) -> Event {
    tokio::time::timeout(tokio::time::Duration::from_secs(secs), rx.recv_async())
        .await
        .expect("Timed out waiting for service event")
        .expect("Service event channel closed")
}

/// Builds a BPA as node ipn:0.1 with an ingress CLA, and dispatches an
/// inbound bundle from ipn:0.2.1 addressed to the local service ipn:0.1.7.
async fn bpa_with_inbound(payload: &[u8]) -> (Bpa, Bytes) {
    let node_ids = hardy_bpa::node_ids::NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false);

    let inbound = build_bundle(
        &"ipn:0.2.1".parse().unwrap(),
        &"ipn:0.1.7".parse().unwrap(),
        payload,
    );

    let cla = IngressCla::new();
    bpa.register_cla("ingress".to_string(), cla.clone(), None)
        .await
        .unwrap();
    cla.sink
        .get()
        .unwrap()
        .dispatch(inbound.clone(), None, None)
        .await
        .unwrap();

    (bpa, inbound)
}

// ---------------------------------------------------------------------------
// Full-path tests: deliver_bundle -> on_deliver_streamed
// ---------------------------------------------------------------------------

/// A streaming service receives the whole bundle as a single `Final`
/// segment with an exact `total_len`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streaming_service_receives_single_final_segment() {
    let node_ids = hardy_bpa::node_ids::NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false);

    let (svc, events_rx) = StreamingService::new(false);
    bpa.register_service(hardy_bpv7::eid::Service::Ipn(7), svc.clone())
        .await
        .unwrap();

    let cla = IngressCla::new();
    bpa.register_cla("ingress".to_string(), cla.clone(), None)
        .await
        .unwrap();
    cla.sink
        .get()
        .unwrap()
        .dispatch(
            build_bundle(
                &"ipn:0.2.1".parse().unwrap(),
                &"ipn:0.1.7".parse().unwrap(),
                b"ping",
            ),
            None,
            None,
        )
        .await
        .unwrap();

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
        .expect("Failed to parse delivered bundle");
    assert_eq!(parsed.bundle.id.source, "ipn:0.2.1".parse().unwrap());
    assert_eq!(parsed.bundle.destination, "ipn:0.1.7".parse().unwrap());

    assert!(events_rx.is_empty());
    bpa.shutdown().await;
}

/// A service that only implements `on_deliver` still receives the whole
/// bundle, through the buffered default adapter.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn buffered_service_receives_whole_bundle_via_default_adapter() {
    let (bpa, _inbound) = bpa_with_inbound(b"pong").await;

    let (svc, events_rx) = BufferedService::new();
    bpa.register_service(hardy_bpv7::eid::Service::Ipn(7), svc.clone())
        .await
        .unwrap();

    let Event::Received(data) = recv_event(&events_rx, 5).await else {
        panic!("Expected the buffered adapter to call on_deliver");
    };
    let parsed = hardy_bpv7::bundle::ParsedBundle::parse(&data, hardy_bpv7::bpsec::no_keys)
        .expect("Failed to parse delivered bundle");
    assert_eq!(parsed.bundle.destination, "ipn:0.1.7".parse().unwrap());

    bpa.shutdown().await;
}

/// A failed streamed delivery parks the bundle as WaitingForService, and a
/// subsequent registration on the same EID re-delivers it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_streamed_delivery_parks_and_redelivers() {
    let (bpa, _inbound) = bpa_with_inbound(b"park me").await;

    let (failing_svc, failing_rx) = StreamingService::new(true);
    bpa.register_service(hardy_bpv7::eid::Service::Ipn(7), failing_svc.clone())
        .await
        .unwrap();
    assert!(matches!(recv_event(&failing_rx, 5).await, Event::Failed));

    failing_svc.sink.get().unwrap().unregister().await;

    let (svc, events_rx) = StreamingService::new(false);
    bpa.register_service(hardy_bpv7::eid::Service::Ipn(7), svc.clone())
        .await
        .unwrap();

    let Event::Streamed { segments, .. } = recv_event(&events_rx, 10).await else {
        panic!("Expected re-delivery through the streamed door");
    };
    assert!(matches!(segments.last(), Some(Segment::Final(_))));

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// Direct-call tests: the default adapter body
// ---------------------------------------------------------------------------

fn expiry() -> time::OffsetDateTime {
    time::OffsetDateTime::now_utc() + time::Duration::hours(1)
}

/// The default body reassembles a multi-segment stream before delegating.
#[tokio::test]
async fn default_body_concats_multi_segment_stream() {
    let (svc, events_rx) = BufferedService::new();
    let data = build_bundle(
        &"ipn:0.1.1".parse().unwrap(),
        &"ipn:0.2.99".parse().unwrap(),
        b"payload",
    );
    let (head, tail) = (data.slice(..10), data.slice(10..));
    let rx = feed(vec![Segment::Next(head), Segment::Final(tail)]).await;

    services::Service::on_deliver_streamed(
        &*svc,
        &bundle_id_of(&data),
        &rx,
        expiry(),
        data.len() as u64,
    )
    .await
    .unwrap();

    let Ok(Event::Received(received)) = events_rx.try_recv() else {
        panic!("Expected on_deliver to get the reassembled bundle");
    };
    assert_eq!(received, data);
}

/// A single-`Final` stream passes through the default body zero-copy.
#[tokio::test]
async fn default_body_is_zero_copy_for_single_final() {
    let (svc, events_rx) = BufferedService::new();
    let data = build_bundle(
        &"ipn:0.1.1".parse().unwrap(),
        &"ipn:0.2.99".parse().unwrap(),
        b"payload",
    );
    let rx = feed(vec![Segment::Final(data.clone())]).await;

    services::Service::on_deliver_streamed(
        &*svc,
        &bundle_id_of(&data),
        &rx,
        expiry(),
        data.len() as u64,
    )
    .await
    .unwrap();

    let Ok(Event::Received(received)) = events_rx.try_recv() else {
        panic!("Expected on_deliver to get the bundle");
    };
    assert_eq!(received.as_ptr(), data.as_ptr());
}

/// A truncated stream is an error, and `on_deliver` is never invoked — no
/// partial bundle reaches the service.
#[tokio::test]
async fn default_body_truncated_stream_is_cancelled() {
    let (svc, events_rx) = BufferedService::new();
    let data = build_bundle(
        &"ipn:0.1.1".parse().unwrap(),
        &"ipn:0.2.99".parse().unwrap(),
        b"payload",
    );
    let rx = feed(vec![Segment::Next(data.slice(..4))]).await;

    let Err(err) = services::Service::on_deliver_streamed(
        &*svc,
        &bundle_id_of(&data),
        &rx,
        expiry(),
        data.len() as u64,
    )
    .await
    else {
        panic!("Expected a truncated stream to fail");
    };
    assert!(matches!(err, services::Error::StreamCancelled));
    assert!(events_rx.is_empty());
}

/// A stream completing with fewer bytes than the declared `total_len` is
/// rejected before delegation — no short bundle reaches the service.
#[tokio::test]
async fn default_body_rejects_under_delivering_stream() {
    let (svc, events_rx) = BufferedService::new();
    let data = build_bundle(
        &"ipn:0.1.1".parse().unwrap(),
        &"ipn:0.2.99".parse().unwrap(),
        b"payload",
    );
    let short = data.slice(..data.len() - 1);
    let rx = feed(vec![Segment::Final(short.clone())]).await;

    let Err(err) = services::Service::on_deliver_streamed(
        &*svc,
        &bundle_id_of(&data),
        &rx,
        expiry(),
        data.len() as u64,
    )
    .await
    else {
        panic!("Expected an under-delivering stream to fail");
    };
    assert!(matches!(
        err,
        services::Error::PayloadUnderrun { size, expected }
            if size == short.len() && expected == data.len()
    ));
    assert!(events_rx.is_empty());
}

/// A stream exceeding the declared `total_len` is rejected before
/// delegation.
#[tokio::test]
async fn default_body_rejects_stream_exceeding_total_len() {
    let (svc, events_rx) = BufferedService::new();
    let data = build_bundle(
        &"ipn:0.1.1".parse().unwrap(),
        &"ipn:0.2.99".parse().unwrap(),
        b"payload",
    );
    let rx = feed(vec![Segment::Final(data.clone())]).await;

    let Err(err) =
        services::Service::on_deliver_streamed(&*svc, &bundle_id_of(&data), &rx, expiry(), 4).await
    else {
        panic!("Expected an oversize stream to fail");
    };
    assert!(matches!(
        err,
        services::Error::PayloadTooLarge { size, max: 4 } if size > 4
    ));
    assert!(events_rx.is_empty());
}
