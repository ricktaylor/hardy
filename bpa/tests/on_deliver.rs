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
    status_report::{AdministrativeRecord, BundleStatusReport, ReasonCode},
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

/// Holds every delivery open until released, then either drains the stream
/// and completes `Ok` or defers with an error: a delivery deliberately
/// still in flight across the bundle's expiry.
struct HoldingService {
    sink: hardy_async::sync::spin::Once<Box<dyn services::ServiceSink>>,
    failing: bool,
    started_tx: flume::Sender<()>,
    release_rx: flume::Receiver<()>,
}

impl HoldingService {
    fn new(failing: bool) -> (Arc<Self>, flume::Receiver<()>, flume::Sender<()>) {
        let (started_tx, started_rx) = flume::bounded(1);
        let (release_tx, release_rx) = flume::bounded(1);
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                failing,
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
        // send_async: a sync flume send here can park a runtime worker if a
        // regression ever produces a duplicate delivery, wedging the whole
        // runtime (timers included) instead of failing the test.
        let _ = self.started_tx.send_async(()).await;
        let _ = self.release_rx.recv_async().await;
        if self.failing {
            return Err(services::Error::StreamCancelled);
        }
        // The race under test is the resolution claim, not the stream: the
        // delivery's buffer was loaded before the bundle expired, so it
        // still drains to Final.
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
// Helpers
// ---------------------------------------------------------------------------

fn build_bundle(source: &Eid, destination: &Eid, payload: &[u8]) -> Bytes {
    let (_, data) = Builder::new(source.clone(), destination.clone())
        .with_payload(Cow::Borrowed(payload))
        .build(CreationTimestamp::now())
        .expect("Failed to build bundle");
    Bytes::from(data)
}

/// The identity of a bundle built by [`build_bundle`], for direct
/// `on_deliver` calls.
fn bundle_id_of(data: &Bytes) -> Id {
    parse(data.clone())
        .expect("Failed to parse built bundle")
        .bundle
        .primary
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
    bpa.start(false).await;

    let inbound = build_bundle(
        &"ipn:0.2.1".parse().unwrap(),
        &"ipn:0.1.7".parse().unwrap(),
        payload,
    );

    let cla = IngressCla::new();
    bpa.register_cla("ingress".to_string(), cla.clone(), None, None)
        .await
        .unwrap();
    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(None, None, &mut inbound.clone())
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

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
    bpa.start(false).await;

    let (svc, events_rx) = StreamingService::new(false);
    bpa.register_service(Service::Ipn(7), svc.clone())
        .await
        .unwrap();

    let cla = IngressCla::new();
    bpa.register_cla("ingress".to_string(), cla.clone(), None, None)
        .await
        .unwrap();
    assert_eq!(
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
            .unwrap(),
        cla::Acceptance::Accepted
    );

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

    // A single fresh registration suffices: the failed delivery's park
    // re-checks the routing snapshot, so whichever side the park lands on —
    // before this registration's WaitingForService poll, or after it (the
    // registration changed the table mid-flight) — the bundle re-enters
    // dispatch and reaches the new service. The timeout only bounds a
    // regression.
    let (svc, events_rx) = StreamingService::new(false);
    bpa.register_service(Service::Ipn(7), svc.clone())
        .await
        .unwrap();
    let Event::Streamed { segments, .. } = recv_event(&events_rx, 10).await else {
        panic!("Expected re-delivery through the streamed door");
    };
    assert!(matches!(segments.last(), Some(Segment::Final(_))));

    bpa.shutdown().await;
}

/// Decode the status report carried by a captured report bundle.
fn decode_report(segments: &[Segment]) -> BundleStatusReport {
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
    status
}

/// Shared rig for the mid-delivery expiry tests: node ipn:0.1 with report
/// capture at ipn:0.1.9; bundle A (ipn:0.2.1 → ipn:0.1.7, 2 s lifetime,
/// deletion report requested) fails a first delivery — parking it as
/// WaitingForService, on the reaper's watch — and is then re-delivered to a
/// [`HoldingService`] that holds it open across its expiry. A companion
/// bundle B (ipn:0.2.2 → ipn:0.1.6, same lifetime, created after A) has no
/// service at all, so it parks as WaitingForService at dispatch and is
/// reaped normally: B's LifetimeExpired deletion report is the
/// deterministic signal that the reaper's expiry pass has run past A's
/// (earlier) expiry — and popped A's watch entry — while A was verifiably
/// still held. Had the reaper wrongly reaped in-flight A, A expires first,
/// so A's report would arrive *before* B's and fail the first source
/// assertion.
async fn expiry_mid_delivery_rig(
    holding_fails: bool,
) -> (
    Bpa,
    Arc<MetadataMemStorage>,
    flume::Receiver<Event>,
    flume::Sender<()>,
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
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .status_reports(true)
        .metadata_storage(metadata_store.clone())
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    // Deletion reports are addressed to ipn:0.1.9: a local capture service.
    let (reports, reports_rx) = StreamingService::new(false);
    bpa.register_service(Service::Ipn(9), reports.clone())
        .await
        .unwrap();

    // A first delivery attempt fails, parking bundle A as
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
    assert!(matches!(recv_event(&failing_rx, 5).await, Event::Failed));

    // Re-register with a service that holds the redelivery open across
    // the bundle's expiry.
    failing_svc.sink.get().unwrap().unregister().await;
    let (svc, started_rx, release_tx) = HoldingService::new(holding_fails);
    bpa.register_service(Service::Ipn(7), svc.clone())
        .await
        .unwrap();

    // The redelivery is in flight (the timeout only bounds a regression)...
    tokio::time::timeout(tokio::time::Duration::from_secs(5), started_rx.recv_async())
        .await
        .expect("Timed out waiting for the delivery to start")
        .expect("Holding service gone");

    // ...now create companion B: local destination, no registered service —
    // it parks as WaitingForService straight from dispatch (never Waiting).
    let (_, data) = Builder::new("ipn:0.2.2".parse().unwrap(), "ipn:0.1.6".parse().unwrap())
        .with_report_to("ipn:0.1.9".parse().unwrap())
        .with_flags(Flags {
            delete_report_requested: true,
            ..Default::default()
        })
        .with_lifetime(Duration::from_millis(2000))
        .with_payload(Cow::Borrowed(b"reap me".as_slice()))
        .build(CreationTimestamp::now())
        .expect("Failed to build bundle");
    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(None, None, &mut Bytes::from(data))
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // The reaper's expiry pass: B — delivery never commenced — is reaped
    // honestly, while held A, in DeliveryAckPending since before the pass,
    // is deferred.
    let Event::Streamed { segments, .. } = recv_event(&reports_rx, 10).await else {
        panic!("Expected the companion's deletion report at report_to");
    };
    let status = decode_report(&segments);
    assert_eq!(
        status.bundle_id.source,
        "ipn:0.2.2".parse::<Eid>().unwrap(),
        "first report must be the never-commenced companion's, not the held delivery's"
    );
    assert!(status.deleted.is_some(), "expected a deletion assertion");
    assert_eq!(status.reason, ReasonCode::LifetimeExpired);

    (bpa, metadata_store, reports_rx, release_tx)
}

/// Assert that shutdown leaves no further reports and no live metadata.
/// shutdown() is the barrier: it joins the pools, so by the time it
/// returns a wrongly-emitted extra report has either been delivered
/// (visible on reports_rx) or persisted-then-stranded when the dispatch
/// channel closed (visible as live metadata). Both are asserted empty; no
/// quiet window is involved.
async fn assert_fully_resolved(
    bpa: Bpa,
    metadata_store: &MetadataMemStorage,
    reports_rx: &flume::Receiver<Event>,
) {
    bpa.shutdown().await;
    assert!(
        reports_rx.is_empty(),
        "an expired bundle was resolved more than once"
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

/// A bundle that expires while its delivery is in flight is resolved
/// exactly once — by the delivery. Once a delivery has commenced the BPA
/// is committed: the service consumes the bundle, so a "deleted: lifetime
/// expired" report would be a lie. The reaper defers the in-flight bundle,
/// and the released completion wins the terminal claim, reporting the
/// deletion as a completed delivery (NoAdditionalInformation) — never
/// LifetimeExpired.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expiry_mid_delivery_resolves_once() {
    let (bpa, metadata_store, reports_rx, release_tx) = expiry_mid_delivery_rig(false).await;

    // Release the held delivery: it completes and claims the resolution.
    release_tx.send(()).expect("Holding service gone");
    let Event::Streamed { segments, .. } = recv_event(&reports_rx, 10).await else {
        panic!("Expected the completed delivery's deletion report at report_to");
    };
    let status = decode_report(&segments);
    assert_eq!(status.bundle_id.source, "ipn:0.2.1".parse::<Eid>().unwrap());
    assert!(status.deleted.is_some(), "expected a deletion assertion");
    assert!(status.received.is_none() && status.delivered.is_none());
    assert_eq!(
        status.reason,
        ReasonCode::NoAdditionalInformation,
        "a consumed delivery must not be reported as expired"
    );

    assert_fully_resolved(bpa, &metadata_store, &reports_rx).await;
}

/// Deferral does not launder expiry: when the held delivery is instead
/// *failed* by the service, nothing was consumed — the park from
/// DeliveryAckPending wins, re-arms the expiry watch, and the reaper then
/// resolves the expired bundle honestly with LifetimeExpired.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_delivery_expires_honestly() {
    let (bpa, metadata_store, reports_rx, release_tx) = expiry_mid_delivery_rig(true).await;

    // Release: the service defers the delivery, and the already-expired
    // bundle is reaped from its park.
    release_tx.send(()).expect("Holding service gone");
    let Event::Streamed { segments, .. } = recv_event(&reports_rx, 10).await else {
        panic!("Expected the deferred bundle's deletion report at report_to");
    };
    let status = decode_report(&segments);
    assert_eq!(status.bundle_id.source, "ipn:0.2.1".parse::<Eid>().unwrap());
    assert!(status.deleted.is_some(), "expected a deletion assertion");
    assert!(status.received.is_none() && status.delivered.is_none());
    assert_eq!(status.reason, ReasonCode::LifetimeExpired);

    assert_fully_resolved(bpa, &metadata_store, &reports_rx).await;
}

// ---------------------------------------------------------------------------
// Direct-call tests: a buffering service over `stream::buffer_stream`
// ---------------------------------------------------------------------------

fn expiry() -> time::OffsetDateTime {
    time::OffsetDateTime::now_utc() + time::Duration::hours(1)
}

/// The buffering path reassembles a multi-segment stream.
#[tokio::test]
async fn buffering_service_concats_multi_segment_stream() {
    let (svc, events_rx) = BufferedService::new();
    let data = build_bundle(
        &"ipn:0.1.1".parse().unwrap(),
        &"ipn:0.2.99".parse().unwrap(),
        b"payload",
    );
    let (head, tail) = (data.slice(..10), data.slice(10..));
    let mut rx = feed(vec![Segment::Next(head), Segment::Final(tail)]).await;

    services::Service::on_deliver(
        &*svc,
        &bundle_id_of(&data),
        expiry(),
        data.len() as u64,
        &mut rx,
    )
    .await
    .unwrap();

    let Ok(Event::Received(received_id, received)) = events_rx.try_recv() else {
        panic!("Expected the buffering service to get the reassembled bundle");
    };
    assert_eq!(received_id, bundle_id_of(&data));
    assert_eq!(received, data);
}

/// A single-`Final` stream passes through the buffering path zero-copy.
#[tokio::test]
async fn buffering_service_is_zero_copy_for_single_final() {
    let (svc, events_rx) = BufferedService::new();
    let data = build_bundle(
        &"ipn:0.1.1".parse().unwrap(),
        &"ipn:0.2.99".parse().unwrap(),
        b"payload",
    );
    let mut rx = feed(vec![Segment::Final(data.clone())]).await;

    services::Service::on_deliver(
        &*svc,
        &bundle_id_of(&data),
        expiry(),
        data.len() as u64,
        &mut rx,
    )
    .await
    .unwrap();

    let Ok(Event::Received(received_id, received)) = events_rx.try_recv() else {
        panic!("Expected the buffering service to get the bundle");
    };
    assert_eq!(received_id, bundle_id_of(&data));
    assert_eq!(received.as_ptr(), data.as_ptr());
}

/// A truncated stream is an error, and no partial bundle reaches the
/// service's consumer.
#[tokio::test]
async fn buffering_service_truncated_stream_is_cancelled() {
    let (svc, events_rx) = BufferedService::new();
    let data = build_bundle(
        &"ipn:0.1.1".parse().unwrap(),
        &"ipn:0.2.99".parse().unwrap(),
        b"payload",
    );
    let mut rx = feed(vec![Segment::Next(data.slice(..4))]).await;

    let Err(err) = services::Service::on_deliver(
        &*svc,
        &bundle_id_of(&data),
        expiry(),
        data.len() as u64,
        &mut rx,
    )
    .await
    else {
        panic!("Expected a truncated stream to fail");
    };
    assert!(matches!(err, services::Error::StreamCancelled));
    assert!(events_rx.is_empty());
}

/// A stream completing with fewer bytes than the declared `total_len` is
/// rejected — no short bundle reaches the service's consumer.
#[tokio::test]
async fn buffering_service_rejects_under_delivering_stream() {
    let (svc, events_rx) = BufferedService::new();
    let data = build_bundle(
        &"ipn:0.1.1".parse().unwrap(),
        &"ipn:0.2.99".parse().unwrap(),
        b"payload",
    );
    let short = data.slice(..data.len() - 1);
    let mut rx = feed(vec![Segment::Final(short.clone())]).await;

    let Err(err) = services::Service::on_deliver(
        &*svc,
        &bundle_id_of(&data),
        expiry(),
        data.len() as u64,
        &mut rx,
    )
    .await
    else {
        panic!("Expected an under-delivering stream to fail");
    };
    assert!(matches!(
        err,
        services::Error::PayloadUnderrun { size, expected }
            if size == short.len() as u64 && expected == data.len() as u64
    ));
    assert!(events_rx.is_empty());
}

/// A stream exceeding the declared `total_len` is rejected.
#[tokio::test]
async fn buffering_service_rejects_stream_exceeding_total_len() {
    let (svc, events_rx) = BufferedService::new();
    let data = build_bundle(
        &"ipn:0.1.1".parse().unwrap(),
        &"ipn:0.2.99".parse().unwrap(),
        b"payload",
    );
    let mut rx = feed(vec![Segment::Final(data.clone())]).await;

    let Err(err) =
        services::Service::on_deliver(&*svc, &bundle_id_of(&data), expiry(), 4, &mut rx).await
    else {
        panic!("Expected an oversize stream to fail");
    };
    assert!(matches!(
        err,
        services::Error::PayloadTooLarge { size, max: 4 } if size > 4
    ));
    assert!(events_rx.is_empty());
}
