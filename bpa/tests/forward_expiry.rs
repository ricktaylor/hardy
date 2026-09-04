//! Integration tests for expiry during a CLA-owned transfer: the reaper
//! defers a bundle in `ForwardAckPending` (the transfer cannot be recalled
//! from the wire), and the bundle resolves truthfully when the outcome
//! arrives — a completed transfer reports completion, and a failed one is
//! dropped as `LifetimeExpired` at the dispatch expiry checkpoint.

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
use std::{borrow::Cow, sync::Arc};

// ---------------------------------------------------------------------------

enum Event {
    /// The capture service pulled a report bundle to completion.
    Streamed { segments: Vec<Segment> },
}

// ---------------------------------------------------------------------------
// Mock service: captures status reports addressed to it
// ---------------------------------------------------------------------------

struct CaptureService {
    sink: hardy_async::sync::spin::Once<Box<dyn services::ServiceSink>>,
    events_tx: flume::Sender<Event>,
}

impl CaptureService {
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
impl services::Service for CaptureService {
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
        let mut segments = Vec::new();
        loop {
            match stream.recv().await {
                Ok(segment @ Segment::Next(_)) => segments.push(segment),
                Ok(segment @ Segment::Final(_)) => {
                    segments.push(segment);
                    break;
                }
                Err(_) => return Err(services::Error::StreamCancelled),
            }
        }
        let _ = self.events_tx.send(Event::Streamed { segments });
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
// Mock CLAs
// ---------------------------------------------------------------------------

/// Buffers the transfer, answers `Accepted`, and reports nothing: the
/// transfer stays owned by the CLA until the test injects the outcome via
/// `Sink::transfer_outcome`.
struct AcceptingCla {
    sink: hardy_async::sync::spin::Once<Box<dyn cla::Sink>>,
    accepted_tx: flume::Sender<Id>,
}

impl AcceptingCla {
    fn new() -> (Arc<Self>, flume::Receiver<Id>) {
        let (tx, rx) = flume::bounded(1);
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                accepted_tx: tx,
            }),
            rx,
        )
    }
}

#[async_trait]
impl cla::Cla for AcceptingCla {
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
        bundle_id: &Id,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> cla::Result<cla::ForwardBundleResult> {
        buffer_stream(stream, total_len).await?;
        let _ = self.accepted_tx.send_async(bundle_id.clone()).await;
        Ok(cla::ForwardBundleResult::Accepted)
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

async fn recv_event(rx: &flume::Receiver<Event>, secs: u64) -> Event {
    // Event-driven wait; the timeout only bounds a regression.
    tokio::time::timeout(tokio::time::Duration::from_secs(secs), rx.recv_async())
        .await
        .expect("Timed out waiting for a captured report")
        .expect("Capture service gone")
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

/// Shared rig for the mid-transfer expiry tests: node ipn:0.1 with report
/// capture at ipn:0.1.9; bundle A (ipn:0.2.1 → ipn:0.3.1, 2 s lifetime,
/// deletion report requested) is forwarded to an [`AcceptingCla`] that owns
/// the transfer across the bundle's expiry. A companion bundle B
/// (ipn:0.2.2 → ipn:0.1.6, same lifetime, created after A) has no service,
/// so it parks as WaitingForService at dispatch and is reaped normally: B's
/// LifetimeExpired deletion report is the deterministic signal that the
/// reaper's expiry pass has run past A's (earlier) expiry — and popped A's
/// watch entry — while A's transfer was verifiably still open. Had the
/// reaper wrongly reaped mid-transfer A, A expires first, so A's report
/// would arrive *before* B's and fail the first source assertion.
async fn expiry_mid_transfer_rig() -> (
    Bpa,
    Arc<MetadataMemStorage>,
    flume::Receiver<Event>,
    Arc<AcceptingCla>,
    Id,
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
    let (reports, reports_rx) = CaptureService::new();
    bpa.register_service(Service::Ipn(9), reports.clone())
        .await
        .unwrap();

    // The egress CLA owns transfers to node ipn:0.3 without resolving them.
    let (cla, accepted_rx) = AcceptingCla::new();
    bpa.register_cla("accepting".to_string(), cla.clone(), None, None)
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

    let (bundle_a, data) = Builder::new("ipn:0.2.1".parse().unwrap(), "ipn:0.3.1".parse().unwrap())
        .with_report_to("ipn:0.1.9".parse().unwrap())
        .with_flags(Flags {
            delete_report_requested: true,
            ..Default::default()
        })
        .with_lifetime(Duration::from_millis(2000))
        .with_payload(Cow::Borrowed(b"expire me".as_slice()))
        .build(CreationTimestamp::now())
        .expect("Failed to build bundle");
    let a_id = bundle_a.primary.id;
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

    // The transfer is accepted and left open (the timeout only bounds a
    // regression): A is in ForwardAckPending.
    let accepted_id = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        accepted_rx.recv_async(),
    )
    .await
    .expect("Timed out waiting for the transfer to be accepted")
    .expect("Accepting CLA gone");
    assert_eq!(accepted_id, a_id);

    // Now create companion B: local destination, no registered service — it
    // parks as WaitingForService straight from dispatch.
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
        ingress
            .sink
            .get()
            .unwrap()
            .dispatch(None, None, &mut Bytes::from(data))
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // The reaper's expiry pass: B — never handed off — is reaped honestly,
    // while A, in ForwardAckPending since before the pass, is deferred.
    let Event::Streamed { segments } = recv_event(&reports_rx, 10).await;
    let status = decode_report(&segments);
    assert_eq!(
        status.bundle_id.source,
        "ipn:0.2.2".parse::<Eid>().unwrap(),
        "first report must be the parked companion's, not the open transfer's"
    );
    assert!(status.deleted.is_some(), "expected a deletion assertion");
    assert_eq!(status.reason, ReasonCode::LifetimeExpired);

    (bpa, metadata_store, reports_rx, cla, a_id)
}

/// Assert that shutdown leaves no further reports and no live metadata.
/// shutdown() is the barrier: it joins the pools, so by the time it returns
/// a wrongly-emitted extra report has either been delivered (visible on
/// reports_rx) or persisted-then-stranded when the dispatch channel closed
/// (visible as live metadata). Both are asserted empty; no quiet window is
/// involved.
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

/// A bundle that expires while a CLA owns its transfer is resolved exactly
/// once — by the outcome. A completed transfer really was forwarded, so the
/// resolution reports the local deletion as a completed hand-off
/// (NoAdditionalInformation) — never LifetimeExpired.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expiry_mid_transfer_resolves_once() {
    let (bpa, metadata_store, reports_rx, cla, a_id) = expiry_mid_transfer_rig().await;

    // The CLA reports completion: the outcome claims the resolution.
    cla.sink
        .get()
        .unwrap()
        .transfer_outcome(&a_id, cla::TransferOutcome::Completed)
        .await
        .unwrap();
    let Event::Streamed { segments } = recv_event(&reports_rx, 10).await;
    let status = decode_report(&segments);
    assert_eq!(status.bundle_id.source, "ipn:0.2.1".parse::<Eid>().unwrap());
    assert!(status.deleted.is_some(), "expected a deletion assertion");
    assert_eq!(
        status.reason,
        ReasonCode::NoAdditionalInformation,
        "a completed transfer must not be reported as expired"
    );

    assert_fully_resolved(bpa, &metadata_store, &reports_rx).await;
}

/// Deferral does not launder expiry: when the CLA instead reports the
/// deferred transfer *failed*, nothing left the node — the bundle re-enters
/// dispatch, whose expiry checkpoint resolves the expired bundle honestly
/// with LifetimeExpired.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_transfer_expires_at_dispatch() {
    let (bpa, metadata_store, reports_rx, cla, a_id) = expiry_mid_transfer_rig().await;

    cla.sink
        .get()
        .unwrap()
        .transfer_outcome(&a_id, cla::TransferOutcome::Failed)
        .await
        .unwrap();
    let Event::Streamed { segments } = recv_event(&reports_rx, 10).await;
    let status = decode_report(&segments);
    assert_eq!(status.bundle_id.source, "ipn:0.2.1".parse::<Eid>().unwrap());
    assert!(status.deleted.is_some(), "expected a deletion assertion");
    assert_eq!(status.reason, ReasonCode::LifetimeExpired);

    assert_fully_resolved(bpa, &metadata_store, &reports_rx).await;
}
