//! Filter hooks through the real pipeline: what the dispatcher does with each
//! filter outcome, per hook — the disposition table in the `filter` module
//! docs, cell by cell.
//!
//! A correct Rewriter meets a call-time refusal — never the fail-stop — on a
//! bundle whose primary block forbids its edit: the flags are the sender's
//! choice, so an abort there would hand any peer a node-kill.

use core::num::NonZeroU64;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use hardy_bpa::{
    Bytes, async_trait,
    bpa::{Bpa, BpaRegistration},
    builder::BpaBuilder,
    bundle::{Bundle, BundleMetadata, BundleStatus},
    cla,
    filter::{RewriteContext, Rewriter, Verdict, Verifier, pack::FilterPack},
    node_ids::NodeIds,
    services,
    storage::{
        BundleMemStorage, BundleStorage, MetadataMemStorage, MetadataStorage, RecoveryResponse,
        Result as StorageResult,
    },
    stream::{Receiver, Segment, Sender, buffer_stream},
};
// Aliased: the wire bundle and the editor's error, beside the record
// `Bundle` and the bpv7 `Error` imported here.
use hardy_bpv7::{
    Bundle as Bpv7Bundle, Error,
    block::{Flags as BlockFlags, Type},
    builder::Builder,
    bundle::{Flags as BundleFlags, Id},
    crc::CrcType,
    creation_timestamp::CreationTimestamp,
    eid::{Eid, IpnNodeId, NodeId, Service},
    extension_editor::{Error as EditorError, ExtensionEditor},
    parse::parse,
    reader::Reader,
    status_report::{AdministrativeRecord, BundleStatusReport, ReasonCode},
};

// The block type the inserting Rewriter adds.
const INSERTED: Type = Type::Unrecognised(200);

// A remote source (report-to), a remote destination behind the same peer,
// and the local service endpoint.
const SOURCE: &str = "ipn:0.3.1";
const REMOTE: &str = "ipn:0.2.99";
const LOCAL: &str = "ipn:0.1.42";

// What an insert that asks for a status report on failure came to.
#[derive(Debug, PartialEq)]
enum InsertOutcome {
    Inserted,
    // Refused with the parser's error for the forbidden flag.
    RefusedInvalidFlags,
    Other(String),
}

// Inserts one extension block flagged `report_on_failure` — as bpv7's
// own `Builder::with_hop_count` flags its block — and treats a refusal as
// its no-match path, recording every outcome.
struct ReportingInserter {
    outcomes: Arc<Mutex<Vec<InsertOutcome>>>,
}

impl Rewriter for ReportingInserter {
    fn rewrite<'a>(
        &self,
        _bundle: &Bpv7Bundle,
        _reader: &'a dyn Reader<'a>,
        _metadata: &BundleMetadata,
        _context: RewriteContext<'_>,
        editor: &mut ExtensionEditor<'_>,
    ) -> Verdict {
        let outcome = match editor.insert(
            INSERTED,
            BlockFlags {
                report_on_failure: true,
                ..Default::default()
            },
            CrcType::None,
            b"inserted".as_slice().into(),
        ) {
            Ok(_) => InsertOutcome::Inserted,
            Err(EditorError::Invalid(Error::InvalidFlags)) => InsertOutcome::RefusedInvalidFlags,
            Err(e) => InsertOutcome::Other(e.to_string()),
        };
        self.outcomes.lock().unwrap().push(outcome);
        Verdict::Continue(())
    }
}

// Drops the bundles addressed to `destination` with `reason`, signalling each
// drop; passes everything else — the status reports it causes included.
struct DestinationDropper {
    destination: Eid,
    reason: Option<ReasonCode>,
    dropped_tx: flume::Sender<()>,
}

impl Verifier for DestinationDropper {
    fn verify<'a>(
        &self,
        bundle: &Bpv7Bundle,
        _reader: &'a dyn Reader<'a>,
        _metadata: &BundleMetadata,
    ) -> Verdict {
        if bundle.primary.destination != self.destination {
            return Verdict::Continue(());
        }
        let _ = self.dropped_tx.send(());
        Verdict::Drop(self.reason)
    }
}

// Passes every bundle: makes a hook's chain non-empty, so the engine runs its
// decode pass.
struct PassVerifier;

impl Verifier for PassVerifier {
    fn verify<'a>(
        &self,
        _bundle: &Bpv7Bundle,
        _reader: &'a dyn Reader<'a>,
        _metadata: &BundleMetadata,
    ) -> Verdict {
        Verdict::Continue(())
    }
}

// Bundle storage the tests can see into: counts saves (a status report is
// saved before it is queued, so every report the node originates is one
// save) and — when `truncate` is set — stores every bundle one byte short,
// so the stored bytes fail to decode.
struct ObservedStorage {
    inner: BundleMemStorage,
    truncate: bool,
    saves: AtomicUsize,
}

impl ObservedStorage {
    fn new(truncate: bool) -> Arc<Self> {
        Arc::new(Self {
            inner: BundleMemStorage::new(None, None),
            truncate,
            saves: AtomicUsize::new(0),
        })
    }

    fn saves(&self) -> usize {
        self.saves.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl BundleStorage for ObservedStorage {
    async fn recover(&self, stream: &dyn Sender<RecoveryResponse>) -> StorageResult<()> {
        self.inner.recover(stream).await
    }

    async fn load(&self, storage_name: &str) -> StorageResult<Option<Bytes>> {
        self.inner.load(storage_name).await
    }

    async fn save(&self, data: Bytes) -> StorageResult<Arc<str>> {
        self.saves.fetch_add(1, Ordering::SeqCst);
        let data = if self.truncate {
            data.slice(..data.len() - 1)
        } else {
            data
        };
        self.inner.save(data).await
    }

    async fn replace(&self, storage_name: &str, data: Bytes) -> StorageResult<()> {
        self.inner.replace(storage_name, data).await
    }

    async fn delete(&self, storage_name: &str) -> StorageResult<()> {
        self.inner.delete(storage_name).await
    }
}

// Metadata storage that reports every park: each successful swap into
// `Waiting` or `WaitingForService`.
struct ObservedMetadata {
    inner: MetadataMemStorage,
    parked_tx: flume::Sender<BundleStatus>,
}

#[async_trait]
impl MetadataStorage for ObservedMetadata {
    async fn get(&self, bundle_id: &Id) -> StorageResult<Option<Bundle>> {
        self.inner.get(bundle_id).await
    }

    async fn insert(&self, bundle: &Bundle) -> StorageResult<bool> {
        self.inner.insert(bundle).await
    }

    async fn swap_status(
        &self,
        bundle_id: &Id,
        expected: &BundleStatus,
        status: &BundleStatus,
    ) -> StorageResult<bool> {
        let swapped = self.inner.swap_status(bundle_id, expected, status).await?;
        if swapped
            && matches!(
                status,
                BundleStatus::Waiting | BundleStatus::WaitingForService { .. }
            )
        {
            let _ = self.parked_tx.send(status.clone());
        }
        Ok(swapped)
    }

    async fn tombstone_if(&self, bundle_id: &Id, expected: &BundleStatus) -> StorageResult<bool> {
        self.inner.tombstone_if(bundle_id, expected).await
    }

    async fn tombstone(&self, bundle_id: &Id) -> StorageResult<()> {
        self.inner.tombstone(bundle_id).await
    }

    async fn start_recovery(&self) {
        self.inner.start_recovery().await
    }

    async fn confirm_exists(
        &self,
        bundle_id: &Id,
    ) -> StorageResult<Option<(BundleMetadata, BundleStatus)>> {
        self.inner.confirm_exists(bundle_id).await
    }

    async fn remove_unconfirmed(&self, stream: &dyn Sender<Bundle>) -> StorageResult<()> {
        self.inner.remove_unconfirmed(stream).await
    }

    async fn reset_peer_queue(&self, peer: u32) -> StorageResult<u64> {
        self.inner.reset_peer_queue(peer).await
    }

    async fn reset_peer_ack_pending(&self, peer: u32) -> StorageResult<u64> {
        self.inner.reset_peer_ack_pending(peer).await
    }

    async fn reset_service_queue(&self, service: &Eid) -> StorageResult<u64> {
        self.inner.reset_service_queue(service).await
    }

    async fn poll_expiry(&self, stream: &dyn Sender<Bundle>) -> StorageResult<()> {
        self.inner.poll_expiry(stream).await
    }

    async fn poll_waiting(&self, stream: &dyn Sender<Bundle>) -> StorageResult<()> {
        self.inner.poll_waiting(stream).await
    }

    async fn poll_service_waiting(
        &self,
        source: Eid,
        stream: &dyn Sender<Bundle>,
    ) -> StorageResult<()> {
        self.inner.poll_service_waiting(source, stream).await
    }

    async fn poll_adu_fragments(
        &self,
        stream: &dyn Sender<Bundle>,
        status: &BundleStatus,
    ) -> StorageResult<()> {
        self.inner.poll_adu_fragments(stream, status).await
    }

    async fn poll_pending(
        &self,
        stream: &dyn Sender<Bundle>,
        status: &BundleStatus,
        limit: usize,
    ) -> StorageResult<()> {
        self.inner.poll_pending(stream, status, limit).await
    }
}

struct CapturingCla {
    sink: hardy_async::sync::spin::Once<Box<dyn cla::Sink>>,
    forwarded_tx: flume::Sender<Bytes>,
}

#[async_trait]
impl cla::Cla for CapturingCla {
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
        _bundle_id: &Id,
        total_len: u64,
        stream: &mut dyn Receiver<cla::Segment>,
    ) -> cla::Result<cla::ForwardBundleResult> {
        let bundle = buffer_stream(stream, total_len).await?;
        let _ = self.forwarded_tx.send(bundle);
        Ok(cla::ForwardBundleResult::Sent)
    }
}

// A raw-bundle service: sees the rewritten extension blocks.
struct CapturingService {
    // Held: dropping the sink unregisters the service.
    sink: hardy_async::sync::spin::Once<Box<dyn services::ServiceSink>>,
    delivered_tx: flume::Sender<Bytes>,
}

#[async_trait]
impl services::Service for CapturingService {
    async fn on_register(&self, _endpoint: &Eid, sink: Box<dyn services::ServiceSink>) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn on_deliver(
        &self,
        _bundle_id: &Id,
        _expiry: time::OffsetDateTime,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        let bundle = buffer_stream(stream, total_len).await?;
        let _ = self.delivered_tx.send(bundle);
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

fn node(node_number: u32) -> NodeId {
    NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number,
    })
}

// Builds and starts `builder` as ipn:0.1, with a CLA whose one peer serves
// ipn:0.2 and ipn:0.3 — the route out for forwarded bundles and for the
// reports to the source alike.
async fn setup(builder: BpaBuilder) -> (Bpa, Arc<CapturingCla>, flume::Receiver<Bytes>) {
    let node_ids = NodeIds::try_from([node(1)].as_slice()).unwrap();
    let bpa = builder.node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    let (forwarded_tx, forwarded_rx) = flume::bounded(16);
    let cla = Arc::new(CapturingCla {
        sink: hardy_async::sync::spin::Once::new(),
        forwarded_tx,
    });
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();
    cla.sink
        .get()
        .unwrap()
        .add_peer(
            cla::ClaAddress::Private("peer".as_bytes().into()),
            &[node(2), node(3)],
        )
        .await
        .unwrap();
    (bpa, cla, forwarded_rx)
}

// Registers a capturing raw service at `LOCAL`. The returned service must
// be held for the test's duration: the registry does not keep it alive.
async fn register_service(bpa: &Bpa) -> (Arc<CapturingService>, flume::Receiver<Bytes>) {
    let (delivered_tx, delivered_rx) = flume::bounded(16);
    let service = Arc::new(CapturingService {
        sink: hardy_async::sync::spin::Once::new(),
        delivered_tx,
    });
    bpa.register_service(Service::Ipn(42), service.clone())
        .await
        .unwrap();
    (service, delivered_rx)
}

// Hands a bundle to the BPA as an arrival from ipn:0.2, returning its id.
async fn arrive(cla: &CapturingCla, source: &str, flags: BundleFlags, destination: &str) -> Id {
    let (built, data) = Builder::new(source.parse().unwrap(), destination.parse().unwrap())
        .with_flags(flags)
        .with_payload(b"payload".as_slice().into())
        .build(CreationTimestamp::now())
        .unwrap();
    let mut inbound = Bytes::from(data);
    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(Some(&node(2)), None, &mut inbound)
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );
    built.primary.id
}

// Waits for the next item on `rx`.
async fn next<T>(rx: &flume::Receiver<T>) -> T {
    // Event-driven wait; the timeout only bounds a regression.
    tokio::time::timeout(tokio::time::Duration::from_secs(5), rx.recv_async())
        .await
        .expect("Timeout waiting for the pipeline")
        .expect("Channel closed")
}

// The next bundle on `rx`, parsed: whether it carries the inserted block.
async fn next_carries_inserted(rx: &flume::Receiver<Bytes>) -> bool {
    let parsed = parse(next(rx).await).expect("the bundle leaving the node must parse");
    parsed
        .bundle
        .blocks
        .values()
        .any(|b| b.block_type == INSERTED)
}

// The next bundle on `rx`, which must be a status report.
async fn next_report(rx: &flume::Receiver<Bytes>) -> BundleStatusReport {
    let parsed = parse(next(rx).await).expect("the report must parse");
    assert!(
        parsed.bundle.primary.flags.is_admin_record,
        "only a status report may leave the node"
    );
    let body = parsed.bundle.blocks[&1]
        .payload(&parsed.data)
        .expect("the report payload is resident");
    let AdministrativeRecord::BundleStatusReport(report) =
        hardy_cbor::decode::parse(body).expect("the payload is an administrative record");
    report
}

fn report_requested() -> BundleFlags {
    BundleFlags {
        delete_report_requested: true,
        ..Default::default()
    }
}

// A BPA with status reports on, observed storage, and a Verifier at the
// given output hook that drops the bundles addressed to `destination` with
// `reason`: (bpa, cla, forwarded, drops, storage, metadata).
async fn dropping_setup(
    deliver: bool,
    destination: &str,
    reason: Option<ReasonCode>,
) -> (
    Bpa,
    Arc<CapturingCla>,
    flume::Receiver<Bytes>,
    flume::Receiver<()>,
    Arc<ObservedStorage>,
    Arc<MetadataMemStorage>,
) {
    let (dropped_tx, dropped_rx) = flume::unbounded();
    let dropper = DestinationDropper {
        destination: destination.parse().unwrap(),
        reason,
        dropped_tx,
    };
    let mut pack = FilterPack::new("test");
    if deliver {
        pack.deliver_verifier("dropper", dropper);
    } else {
        pack.egress_verifier("dropper", dropper);
    }
    let storage = ObservedStorage::new(false);
    let metadata = Arc::new(MetadataMemStorage::new(None));
    let (bpa, cla, forwarded_rx) = setup(
        Bpa::builder()
            .add_filters(pack)
            .status_reports(true)
            .bundle_storage(storage.clone())
            .no_cache()
            .metadata_storage(metadata.clone()),
    )
    .await;
    (bpa, cla, forwarded_rx, dropped_rx, storage, metadata)
}

/// CRIT-1 at Egress: a transit bundle with only `is_admin_record` set — no
/// local registration involved — forbids the Rewriter's flag. The insert is
/// refused with the parser's error and the bundle is forwarded unedited.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn egress_rewriter_meets_a_refusal_on_an_admin_record() {
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let mut pack = FilterPack::new("test");
    pack.egress_rewriter(
        "inserter",
        ReportingInserter {
            outcomes: outcomes.clone(),
        },
    );
    let (bpa, cla, forwarded_rx) = setup(Bpa::builder().add_filters(pack)).await;

    // Control: an ordinary transit bundle takes the insert.
    arrive(&cla, SOURCE, BundleFlags::default(), REMOTE).await;
    assert!(next_carries_inserted(&forwarded_rx).await);

    let admin_record = BundleFlags {
        is_admin_record: true,
        ..Default::default()
    };
    arrive(&cla, SOURCE, admin_record, REMOTE).await;
    assert!(!next_carries_inserted(&forwarded_rx).await);

    bpa.shutdown().await;
    assert_eq!(
        *outcomes.lock().unwrap(),
        [InsertOutcome::Inserted, InsertOutcome::RefusedInvalidFlags]
    );
}

/// CRIT-1 at Deliver: an anonymous (null-source) bundle forbids the
/// Rewriter's flag. The insert is refused with the parser's error and the
/// bundle is delivered unedited.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deliver_rewriter_meets_a_refusal_on_a_null_source() {
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let mut pack = FilterPack::new("test");
    pack.deliver_rewriter(
        "inserter",
        ReportingInserter {
            outcomes: outcomes.clone(),
        },
    );
    let (bpa, cla, _forwarded_rx) = setup(Bpa::builder().add_filters(pack)).await;
    let (_service, delivered_rx) = register_service(&bpa).await;

    // Control: an ordinary arrival takes the insert.
    arrive(&cla, SOURCE, BundleFlags::default(), LOCAL).await;
    assert!(next_carries_inserted(&delivered_rx).await);

    // A null source must also forbid fragmentation (RFC 9171 §4.2.3-4).
    let anonymous = BundleFlags {
        do_not_fragment: true,
        ..Default::default()
    };
    arrive(&cla, "dtn:none", anonymous, LOCAL).await;
    assert!(!next_carries_inserted(&delivered_rx).await);

    bpa.shutdown().await;
    assert_eq!(
        *outcomes.lock().unwrap(),
        [InsertOutcome::Inserted, InsertOutcome::RefusedInvalidFlags]
    );
}

/// Egress `Drop(Some(reason))`: one deletion report carrying the filter's
/// reason; the bundle is not transmitted and its record is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn egress_drop_with_a_reason_reports_once() {
    let (bpa, cla, forwarded_rx, _dropped_rx, storage, metadata) =
        dropping_setup(false, REMOTE, Some(ReasonCode::TrafficPared)).await;

    let id = arrive(&cla, SOURCE, report_requested(), REMOTE).await;
    let report = next_report(&forwarded_rx).await;
    assert_eq!(report.bundle_id, id);
    assert!(report.deleted.is_some());
    assert_eq!(report.reason, ReasonCode::TrafficPared);

    // The completed shutdown is the barrier proving the absences: the drop's
    // resolution — which saves any report it originates — has finished.
    bpa.shutdown().await;
    assert!(
        forwarded_rx.is_empty(),
        "neither the bundle nor a second report may leave the node"
    );
    assert_eq!(storage.saves(), 2, "the arrival and exactly one report");
    assert!(metadata.get(&id).await.unwrap().is_none());
}

/// Egress `Drop(None)`: silent even though the bundle requests deletion
/// reports; the bundle is not transmitted and its record is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn egress_drop_without_a_reason_is_silent() {
    let (bpa, cla, forwarded_rx, dropped_rx, storage, metadata) =
        dropping_setup(false, REMOTE, None).await;

    let id = arrive(&cla, SOURCE, report_requested(), REMOTE).await;
    next(&dropped_rx).await;

    // The completed shutdown is the barrier proving the absences: the drop's
    // resolution — which would save any report it originates — has finished.
    bpa.shutdown().await;
    assert!(forwarded_rx.is_empty(), "the bundle is not transmitted");
    assert_eq!(storage.saves(), 1, "the arrival only: no report originated");
    assert!(metadata.get(&id).await.unwrap().is_none());
}

/// Deliver `Drop(Some(reason))`: one deletion report carrying the filter's
/// reason; the bundle is not delivered and its record is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deliver_drop_with_a_reason_reports_once() {
    let (bpa, cla, forwarded_rx, _dropped_rx, storage, metadata) =
        dropping_setup(true, LOCAL, Some(ReasonCode::TrafficPared)).await;
    let (_service, delivered_rx) = register_service(&bpa).await;

    let id = arrive(&cla, SOURCE, report_requested(), LOCAL).await;
    let report = next_report(&forwarded_rx).await;
    assert_eq!(report.bundle_id, id);
    assert!(report.deleted.is_some());
    assert_eq!(report.reason, ReasonCode::TrafficPared);

    // The completed shutdown is the barrier proving the absences.
    bpa.shutdown().await;
    assert!(delivered_rx.is_empty(), "a dropped bundle is not delivered");
    assert!(
        forwarded_rx.is_empty(),
        "exactly one report leaves the node"
    );
    assert_eq!(storage.saves(), 2, "the arrival and exactly one report");
    assert!(metadata.get(&id).await.unwrap().is_none());
}

/// Deliver `Drop(None)`: silent even though the bundle requests deletion
/// reports; the bundle is not delivered and its record is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deliver_drop_without_a_reason_is_silent() {
    let (bpa, cla, forwarded_rx, dropped_rx, storage, metadata) =
        dropping_setup(true, LOCAL, None).await;
    let (_service, delivered_rx) = register_service(&bpa).await;

    let id = arrive(&cla, SOURCE, report_requested(), LOCAL).await;
    next(&dropped_rx).await;

    // The completed shutdown is the barrier proving the absences.
    bpa.shutdown().await;
    assert!(delivered_rx.is_empty(), "a dropped bundle is not delivered");
    assert!(forwarded_rx.is_empty(), "no report leaves the node");
    assert_eq!(storage.saves(), 1, "the arrival only: no report originated");
    assert!(metadata.get(&id).await.unwrap().is_none());
}

// A BPA whose stored bundles do not decode, with a pass-through Verifier at
// the given output hook: (bpa, cla, forwarded, parks, metadata).
async fn undecodable_setup(
    deliver: bool,
) -> (
    Bpa,
    Arc<CapturingCla>,
    flume::Receiver<Bytes>,
    flume::Receiver<BundleStatus>,
    Arc<ObservedMetadata>,
) {
    let mut pack = FilterPack::new("test");
    if deliver {
        pack.deliver_verifier("pass", PassVerifier);
    } else {
        pack.egress_verifier("pass", PassVerifier);
    }
    let (parked_tx, parked_rx) = flume::unbounded();
    let metadata = Arc::new(ObservedMetadata {
        inner: MetadataMemStorage::new(None),
        parked_tx,
    });
    let (bpa, cla, forwarded_rx) = setup(
        Bpa::builder()
            .add_filters(pack)
            .bundle_storage(ObservedStorage::new(true))
            .no_cache()
            .metadata_storage(metadata.clone()),
    )
    .await;
    (bpa, cla, forwarded_rx, parked_rx, metadata)
}

/// Egress, stored bytes that do not decode: the claim returns to `Waiting`
/// for a fresh routing decision and nothing is transmitted. (The fixed
/// per-hop rewrite ahead of the chain decodes the stored bytes first, so
/// this is its park; the engine's own decode arm stays defensive.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undecodable_stored_bundle_parks_waiting_at_egress() {
    let (bpa, cla, forwarded_rx, parked_rx, metadata) = undecodable_setup(false).await;

    let id = arrive(&cla, SOURCE, BundleFlags::default(), REMOTE).await;
    assert_eq!(next(&parked_rx).await, BundleStatus::Waiting);

    // The completed shutdown is the barrier proving nothing was sent. (A
    // stale routing poll may re-attempt the parked bundle; it fails the
    // same way.)
    bpa.shutdown().await;
    assert!(forwarded_rx.is_empty(), "nothing is transmitted");
    assert!(
        metadata.get(&id).await.unwrap().is_some(),
        "the record is kept"
    );
}

/// Deliver chain failure — the stored bytes fail the chain's decode pass:
/// the claim parks `WaitingForService` for the next (re-)registration and
/// nothing is delivered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undecodable_stored_bundle_parks_waiting_for_service_at_deliver() {
    let (bpa, cla, _forwarded_rx, parked_rx, metadata) = undecodable_setup(true).await;
    let (_service, delivered_rx) = register_service(&bpa).await;

    let id = arrive(&cla, SOURCE, BundleFlags::default(), LOCAL).await;
    let parked = next(&parked_rx).await;
    assert!(
        matches!(parked, BundleStatus::WaitingForService { .. }),
        "parked as {parked:?}"
    );

    // The completed shutdown is the barrier proving nothing was delivered.
    // (The registration's own post-registration poll may re-attempt the
    // parked bundle; it fails the same way.)
    bpa.shutdown().await;
    assert!(delivered_rx.is_empty(), "nothing is delivered");
    assert!(
        metadata.get(&id).await.unwrap().is_some(),
        "the record is kept"
    );
}
