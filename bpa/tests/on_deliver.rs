//! Integration tests for `Service::on_deliver` — the streamed delivery
//! door — and `stream::buffer_stream`, the whole-buffer convenience used by
//! services that need a contiguous bundle.

use core::{num::NonZeroU32, time::Duration};
use hardy_bpa::{
    Bytes, async_trait,
    bpa::{Bpa, BpaRegistration},
    cla,
    node_ids::NodeIds,
    services,
    storage::{MetadataMemStorage, MetadataStorage},
    stream::{Receiver, Segment, buffer_stream},
};
use hardy_bpv7::{
    builder::Builder,
    bundle::{Flags, Id},
    creation_timestamp::CreationTimestamp,
    eid::{Eid, IpnNodeId, NodeId, Service},
    parse::parse,
    status_report::{AdministrativeRecord, ReasonCode},
};
use std::{
    borrow::Cow,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
// ---------------------------------------------------------------------------

enum Event {
    /// The buffering service assembled the whole bundle.
    Received(Id, Bytes),
    /// The streaming service pulled the stream to completion.
    Streamed {
        bundle_id: Id,
        segments: Vec<Segment>,
        total_len: u64,
    },
    /// The delivery failed.
    Failed,
}

// ---------------------------------------------------------------------------
// Mock services
// ---------------------------------------------------------------------------

/// Consumes the stream segment by segment, recording every segment it pulls.
struct StreamingService {
    sink: hardy_async::sync::spin::Once<Box<dyn services::ServiceSink>>,
    events_tx: flume::Sender<Event>,
    /// When set, every `on_deliver` call fails without pulling.
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
        bundle_id: &Id,
        _expiry: time::OffsetDateTime,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
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
            bundle_id: bundle_id.clone(),
            segments,
            total_len,
        });
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

/// Buffers the stream into a contiguous bundle via `stream::buffer_stream`
/// — the shape every whole-buffer service takes on the streamed-only door.
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
        bundle_id: &Id,
        _expiry: time::OffsetDateTime,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        let data = buffer_stream(stream, total_len).await?;
        let _ = self
            .events_tx
            .send(Event::Received(bundle_id.clone(), data));
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

/// Holds every delivery open until released, then drains the stream and
/// completes `Ok`: a delivery deliberately still in flight when something
/// else (the expiry reaper) resolves the bundle.
struct HoldingService {
    sink: hardy_async::sync::spin::Once<Box<dyn services::ServiceSink>>,
    started_tx: flume::Sender<()>,
    release_rx: flume::Receiver<()>,
}

impl HoldingService {
    fn new() -> (Arc<Self>, flume::Receiver<()>, flume::Sender<()>) {
        let (started_tx, started_rx) = flume::bounded(1);
        let (release_tx, release_rx) = flume::bounded(1);
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                started_tx,
                release_rx,
            }),
            started_rx,
            release_tx,
        )
    }
}

#[async_trait]
impl services::Service for HoldingService {
    async fn on_register(&self, _endpoint: &Eid, sink: Box<dyn services::ServiceSink>) {
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
        let _ = self.started_tx.send(());
        let _ = self.release_rx.recv_async().await;
        // The race under test is the completion claim, not the stream: the
        // delivery's buffer was loaded before the reaper ran, so it still
        // drains to Final.
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
// Helpers
// ---------------------------------------------------------------------------

fn build_bundle(source: &Eid, destination: &Eid, payload: &[u8]) -> Bytes {
    let (_, data) = Builder::new(source.clone(), destination.clone())
        .with_payload(Cow::Borrowed(payload))
        .build(CreationTimestamp::now())
        .expect("Failed to build bundle");
    Bytes::from(data)
}

async fn recv_event(rx: &flume::Receiver<Event>, secs: u64) -> Event {
    // Event-driven wait; the timeout only bounds a regression.
    tokio::time::timeout(tokio::time::Duration::from_secs(secs), rx.recv_async())
        .await
        .expect("Timed out waiting for service event")
        .expect("Service event channel closed")
}

/// Builds a BPA as node ipn:0.1 with an ingress CLA, and dispatches an
/// inbound bundle from ipn:0.2.1 addressed to the local service ipn:0.1.7.
async fn bpa_with_inbound(payload: &[u8]) -> (Bpa, Bytes) {
    let node_ids = NodeIds::try_from(
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
        .dispatch(None, None, &mut inbound.clone())
        .await
        .unwrap();

    (bpa, inbound)
}

// ---------------------------------------------------------------------------
// Full-path tests: deliver_bundle -> on_deliver
// ---------------------------------------------------------------------------

/// A streaming service receives the whole bundle as a single `Final`
/// segment with an exact `total_len`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streaming_service_receives_single_final_segment() {
    let node_ids = NodeIds::try_from(
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
    bpa.register_service(Service::Ipn(7), svc.clone())
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
            None,
            None,
            &mut build_bundle(
                &"ipn:0.2.1".parse().unwrap(),
                &"ipn:0.1.7".parse().unwrap(),
                b"ping",
            ),
        )
        .await
        .unwrap();

    let Event::Streamed {
        bundle_id,
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

    let parsed = parse(data.clone()).expect("Failed to parse delivered bundle");
    assert_eq!(bundle_id, parsed.bundle.primary.id);
    assert_eq!(
        parsed.bundle.primary.id.source,
        "ipn:0.2.1".parse().unwrap()
    );
    assert_eq!(
        parsed.bundle.primary.destination,
        "ipn:0.1.7".parse().unwrap()
    );

    assert!(events_rx.is_empty());
    bpa.shutdown().await;
}

/// A service that buffers via `stream::buffer_stream` still receives the
/// whole bundle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn buffered_service_receives_whole_bundle() {
    let (bpa, _inbound) = bpa_with_inbound(b"pong").await;

    let (svc, events_rx) = BufferedService::new();
    bpa.register_service(Service::Ipn(7), svc.clone())
        .await
        .unwrap();

    let Event::Received(bundle_id, data) = recv_event(&events_rx, 5).await else {
        panic!("Expected the buffering service to assemble the bundle");
    };
    let parsed = parse(data.clone()).expect("Failed to parse delivered bundle");
    assert_eq!(bundle_id, parsed.bundle.primary.id);
    assert_eq!(
        parsed.bundle.primary.destination,
        "ipn:0.1.7".parse().unwrap()
    );

    bpa.shutdown().await;
}

/// A failed streamed delivery parks the bundle as WaitingForService, and a
/// subsequent registration on the same EID re-delivers it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_streamed_delivery_parks_and_redelivers() {
    let (bpa, _inbound) = bpa_with_inbound(b"park me").await;

    let (failing_svc, failing_rx) = StreamingService::new(true);
    bpa.register_service(Service::Ipn(7), failing_svc.clone())
        .await
        .unwrap();
    assert!(matches!(recv_event(&failing_rx, 5).await, Event::Failed));

    failing_svc.sink.get().unwrap().unregister().await;

    // The failed delivery parks the bundle *after* the service reports the
    // failure, so a single registration can race the park; each fresh
    // registration re-triggers the WaitingForService poll.
    let mut redelivery = None;
    for i in 0.. {
        let (svc, events_rx) = StreamingService::new(false);
        bpa.register_service(Service::Ipn(7), svc.clone())
            .await
            .unwrap();
        // Known test-guide deviation: a timed retry loop, not an
        // event-driven wait. Scheduled for rewrite against the Phase 3
        // Deliver seat (see bpa/docs/refactor_plan.md).
        if let Ok(Ok(event)) = tokio::time::timeout(
            tokio::time::Duration::from_millis(500),
            events_rx.recv_async(),
        )
        .await
        {
            redelivery = Some(event);
            break;
        }
        assert!(i < 20, "Timed out waiting for the re-delivery");
        svc.sink.get().unwrap().unregister().await;
    }
    let Some(Event::Streamed { segments, .. }) = redelivery else {
        panic!("Expected re-delivery through the streamed door");
    };
    assert!(matches!(segments.last(), Some(Segment::Final(_))));

    bpa.shutdown().await;
}

/// A bundle that expires while its delivery is in flight is resolved
/// exactly once. The reaper wins the conditional terminal claim and sends
/// the only deletion report; the delivery's completion loses the claim and
/// stays silent instead of contradicting it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expiry_mid_delivery_resolves_once() {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let metadata_store = Arc::new(MetadataMemStorage::new(None));
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .status_reports(true)
        .metadata_storage(metadata_store.clone())
        .build()
        .await
        .unwrap();
    bpa.start(false);

    // Deletion reports are addressed to ipn:0.1.9: a local capture service.
    let (reports, reports_rx) = StreamingService::new(false);
    bpa.register_service(Service::Ipn(9), reports.clone())
        .await
        .unwrap();

    // A first delivery attempt fails, parking the bundle as
    // WaitingForService — which is also what places it on the reaper's
    // expiry watch list.
    let (failing_svc, failing_rx) = StreamingService::new(true);
    bpa.register_service(Service::Ipn(7), failing_svc.clone())
        .await
        .unwrap();

    let (_, data) = Builder::new("ipn:0.2.1".parse().unwrap(), "ipn:0.1.7".parse().unwrap())
        .with_report_to("ipn:0.1.9".parse().unwrap())
        .with_flags(Flags {
            delete_report_requested: true,
            ..Default::default()
        })
        .with_lifetime(Duration::from_millis(2000))
        .with_payload(Cow::Borrowed(b"expire me".as_slice()))
        .build(CreationTimestamp::now())
        .expect("Failed to build bundle");

    let cla = IngressCla::new();
    bpa.register_cla("ingress".to_string(), cla.clone(), None)
        .await
        .unwrap();
    cla.sink
        .get()
        .unwrap()
        .dispatch(None, None, &mut Bytes::from(data))
        .await
        .unwrap();
    assert!(matches!(recv_event(&failing_rx, 5).await, Event::Failed));

    // Re-register with a service that holds the redelivery open across
    // the bundle's expiry.
    failing_svc.sink.get().unwrap().unregister().await;
    let (svc, started_rx, release_tx) = HoldingService::new();
    bpa.register_service(Service::Ipn(7), svc.clone())
        .await
        .unwrap();

    // The redelivery is in flight (the timeout only bounds a regression)...
    tokio::time::timeout(tokio::time::Duration::from_secs(5), started_rx.recv_async())
        .await
        .expect("Timed out waiting for the delivery to start")
        .expect("Holding service gone");

    // ...when the bundle expires: the reaper resolves it, and the deletion
    // report (the only report this bundle requests) reaches report_to.
    let Event::Streamed { segments, .. } = recv_event(&reports_rx, 10).await else {
        panic!("Expected the deletion report at report_to");
    };

    // Pin the winner's identity: the report is the *reaper's* deletion
    // report, citing LifetimeExpired. If the claim logic ever inverted,
    // a delivered/deleted pair from the completion would fail here.
    let Some(Segment::Final(report)) = segments.last() else {
        panic!("Expected a whole report bundle");
    };
    let parsed = parse(report.clone()).expect("Failed to parse report bundle");
    let payload = parsed
        .bundle
        .blocks
        .get(&1)
        .unwrap()
        .payload(&parsed.data)
        .expect("Report payload extent out of bounds");
    let AdministrativeRecord::BundleStatusReport(status) =
        hardy_cbor::decode::parse(payload).expect("Failed to parse admin record");
    assert!(status.deleted.is_some(), "expected a deletion assertion");
    assert!(status.received.is_none() && status.delivered.is_none());
    assert_eq!(status.reason, ReasonCode::LifetimeExpired);

    // Release the held delivery: its completion must lose the terminal
    // claim silently, with no delivered/deleted reporting after the
    // reaper's. shutdown() is the barrier: it joins the pools, so by the
    // time it returns a wrongly-emitted second report has either been
    // delivered (visible on reports_rx) or persisted-then-stranded when
    // the dispatch channel closed (visible as live metadata below).
    // Both are asserted empty; no quiet window is involved.
    release_tx.send(()).expect("Holding service gone");
    bpa.shutdown().await;
    assert!(
        reports_rx.is_empty(),
        "the completed delivery re-resolved the expired bundle"
    );
    let (live_tx, live_rx) = hardy_async::channel::bounded(16);
    metadata_store
        .poll_expiry(&live_tx, 16)
        .await
        .expect("Failed to poll metadata store");
    drop(live_tx);
    assert!(
        live_rx.recv().await.is_err(),
        "a resolved bundle left live metadata behind"
    );
}
