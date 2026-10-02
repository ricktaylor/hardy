//! Filter hooks through the real pipeline: what the dispatcher does with each
//! filter outcome, per hook — the disposition table in the `filter` module
//! docs, cell by cell.
//!
//! A correct Rewriter meets a call-time refusal — never the fail-stop — on a
//! bundle whose primary block forbids its edit: the flags are the sender's
//! choice, so an abort there would hand any peer a node-kill.

use core::{
    num::{NonZeroU64, NonZeroUsize},
    time::Duration,
};
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
    filter::{
        Classifier, RewriteContext, Rewriter, Verdict, Verifier,
        pack::FilterPack,
        slots::{MetadataDelta, SlotHandle},
    },
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

// Inserts one extension block with `flags` and treats a refusal as its
// no-match path, recording every outcome.
struct FlaggedInserter {
    flags: BlockFlags,
    outcomes: Arc<Mutex<Vec<InsertOutcome>>>,
}

// Requests a status report on failure — as bpv7's own
// `Builder::with_hop_count` flags its block.
fn report_on_failure() -> BlockFlags {
    BlockFlags {
        report_on_failure: true,
        ..Default::default()
    }
}

impl Rewriter for FlaggedInserter {
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
            self.flags.clone(),
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

// Writes a registered annotation slot at the Ingress hook.
struct SlotWriter(SlotHandle<u32>, u32);

impl Classifier for SlotWriter {
    fn classify<'a>(
        &self,
        _bundle: &Bpv7Bundle,
        _reader: &'a dyn Reader<'a>,
        _metadata: &BundleMetadata,
    ) -> Verdict<MetadataDelta> {
        let mut delta = MetadataDelta::default();
        delta.set(&self.0, &self.1);
        Verdict::Continue(delta)
    }
}

// Reports the slot's value as the hook it is registered at sees it.
struct SlotReader {
    handle: SlotHandle<u32>,
    seen_tx: flume::Sender<Option<u32>>,
}

impl Verifier for SlotReader {
    fn verify<'a>(
        &self,
        _bundle: &Bpv7Bundle,
        _reader: &'a dyn Reader<'a>,
        metadata: &BundleMetadata,
    ) -> Verdict {
        let _ = self.seen_tx.send(metadata.slot(&self.handle));
        Verdict::Continue(())
    }
}

// An application that only sends.
struct SendingApp {
    // Held: dropping the sink unregisters the application.
    sink: hardy_async::sync::spin::Once<Box<dyn services::ApplicationSink>>,
}

#[async_trait]
impl services::Application for SendingApp {
    async fn on_register(&self, _source: &Eid, sink: Box<dyn services::ApplicationSink>) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn on_deliver(
        &self,
        _bundle_id: &Id,
        _expiry: time::OffsetDateTime,
        _ack_requested: bool,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        buffer_stream(stream, total_len).await?;
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

// The filter hooks, for registering a test Verifier at one of them.
#[derive(Clone, Copy)]
enum Hook {
    Ingress,
    Originate,
    Egress,
    Deliver,
}

// A BPA with status reports on, observed storage, and a Verifier at `hook`
// that drops the bundles addressed to `destination` with `reason`:
// (bpa, cla, forwarded, drops, storage, metadata).
async fn dropping_setup(
    hook: Hook,
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
    match hook {
        Hook::Ingress => pack.ingress_verifier("dropper", dropper),
        Hook::Originate => pack.originate_verifier("dropper", dropper),
        Hook::Egress => pack.egress_verifier("dropper", dropper),
        Hook::Deliver => pack.deliver_verifier("dropper", dropper),
    };
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
        FlaggedInserter {
            flags: report_on_failure(),
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
        FlaggedInserter {
            flags: report_on_failure(),
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

/// The flag encoders keep a named bit out of `unrecognised`: a Rewriter that
/// carries `report_on_failure`'s bit in the raw mask on an anonymous bundle
/// is accepted with the bit dropped, and the bundle is delivered — the
/// re-parse that once aborted the node never sees the bit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deliver_rewriter_raw_report_bit_is_dropped_on_a_null_source() {
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let mut pack = FilterPack::new("test");
    pack.deliver_rewriter(
        "inserter",
        FlaggedInserter {
            flags: BlockFlags {
                unrecognised: Some(1 << 1),
                ..Default::default()
            },
            outcomes: outcomes.clone(),
        },
    );
    let (bpa, cla, _forwarded_rx) = setup(Bpa::builder().add_filters(pack)).await;
    let (_service, delivered_rx) = register_service(&bpa).await;

    let anonymous = BundleFlags {
        do_not_fragment: true,
        ..Default::default()
    };
    arrive(&cla, "dtn:none", anonymous, LOCAL).await;
    let parsed = parse(next(&delivered_rx).await).expect("the delivered bundle must parse");
    let inserted = parsed
        .bundle
        .blocks
        .values()
        .find(|b| b.block_type == INSERTED)
        .expect("the insert was accepted");
    assert!(!inserted.flags.report_on_failure);
    assert_eq!(inserted.flags.unrecognised, None);

    bpa.shutdown().await;
    assert_eq!(*outcomes.lock().unwrap(), [InsertOutcome::Inserted]);
}

/// Egress `Drop(Some(reason))`: one deletion report carrying the filter's
/// reason; the bundle is not transmitted and its record is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn egress_drop_with_a_reason_reports_once() {
    let (bpa, cla, forwarded_rx, _dropped_rx, storage, metadata) =
        dropping_setup(Hook::Egress, REMOTE, Some(ReasonCode::TrafficPared)).await;

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
        dropping_setup(Hook::Egress, REMOTE, None).await;

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
        dropping_setup(Hook::Deliver, LOCAL, Some(ReasonCode::TrafficPared)).await;
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
        dropping_setup(Hook::Deliver, LOCAL, None).await;
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

/// Egress, stored bytes that do not decode: the fixed per-hop rewrite ahead
/// of the chain decodes them first and returns the claim to `Waiting` for a
/// fresh routing decision; nothing is transmitted. This pins the rewrite's
/// park, not an Egress chain failure: the engine's own decode arm is
/// unreachable from storage and stays defensive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undecodable_stored_bundle_parks_waiting_in_the_per_hop_rewrite() {
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

// Registers a sending application at `LOCAL` and sends one report-requesting
// payload to `REMOTE`, returning the send's outcome.
async fn originate(bpa: &Bpa) -> services::Result<Id> {
    let app = Arc::new(SendingApp {
        sink: hardy_async::sync::spin::Once::new(),
    });
    bpa.register_application(Service::Ipn(42), app.clone())
        .await
        .unwrap();
    let options = services::SendOptions {
        notify_reception: true,
        notify_deletion: true,
        ..Default::default()
    };
    app.sink
        .get()
        .unwrap()
        .send(
            REMOTE.parse().unwrap(),
            Bytes::from_static(b"payload"),
            Duration::from_secs(3600),
            Some(options),
        )
        .await
}

fn reception_and_deletion_requested() -> BundleFlags {
    BundleFlags {
        receipt_report_requested: true,
        delete_report_requested: true,
        ..Default::default()
    }
}

/// Ingress `Drop(Some(reason))`: disposed of at the pre-drain gate — the
/// arrival is never stored — with one combined reception + deletion report
/// carrying the filter's reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ingress_drop_with_a_reason_reports_before_storing() {
    let (bpa, cla, forwarded_rx, _dropped_rx, storage, _metadata) =
        dropping_setup(Hook::Ingress, REMOTE, Some(ReasonCode::TrafficPared)).await;

    let id = arrive(&cla, SOURCE, reception_and_deletion_requested(), REMOTE).await;
    let report = next_report(&forwarded_rx).await;
    assert_eq!(report.bundle_id, id);
    assert!(report.received.is_some());
    assert!(report.deleted.is_some());
    assert_eq!(report.reason, ReasonCode::TrafficPared);

    // The completed shutdown is the barrier proving the absences.
    bpa.shutdown().await;
    assert!(forwarded_rx.is_empty(), "the bundle is not forwarded");
    assert_eq!(
        storage.saves(),
        1,
        "the report only: the arrival was never stored"
    );
}

/// Ingress `Drop(None)`: no deletion assertion even though the bundle asks
/// for one, but a requested reception report is still sent; the arrival is
/// never stored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ingress_drop_without_a_reason_reports_reception_only() {
    let (bpa, cla, forwarded_rx, _dropped_rx, storage, _metadata) =
        dropping_setup(Hook::Ingress, REMOTE, None).await;

    let id = arrive(&cla, SOURCE, reception_and_deletion_requested(), REMOTE).await;
    let report = next_report(&forwarded_rx).await;
    assert_eq!(report.bundle_id, id);
    assert!(report.received.is_some());
    assert!(
        report.deleted.is_none(),
        "a silent drop asserts no deletion"
    );

    // The completed shutdown is the barrier proving the absences.
    bpa.shutdown().await;
    assert!(forwarded_rx.is_empty(), "the bundle is not forwarded");
    assert_eq!(
        storage.saves(),
        1,
        "the report only: the arrival was never stored"
    );
}

/// Originate `Drop(Some(reason))`: the reason returns to the sender as
/// `services::Error::Dropped`; nothing is stored and no report is sent,
/// though the send requested them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn originate_drop_with_a_reason_returns_it_to_the_sender() {
    let (bpa, _cla, forwarded_rx, _dropped_rx, storage, _metadata) =
        dropping_setup(Hook::Originate, REMOTE, Some(ReasonCode::TrafficPared)).await;

    assert!(matches!(
        originate(&bpa).await,
        Err(services::Error::Dropped(Some(ReasonCode::TrafficPared)))
    ));

    // The completed shutdown is the barrier proving the absences.
    bpa.shutdown().await;
    assert!(forwarded_rx.is_empty(), "nothing leaves the node");
    assert_eq!(
        storage.saves(),
        0,
        "nothing is stored, no report originated"
    );
}

/// Originate `Drop(None)`: the sender gets `services::Error::Dropped(None)`;
/// nothing is stored and no report is sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn originate_drop_without_a_reason_returns_none_to_the_sender() {
    let (bpa, _cla, forwarded_rx, _dropped_rx, storage, _metadata) =
        dropping_setup(Hook::Originate, REMOTE, None).await;

    assert!(matches!(
        originate(&bpa).await,
        Err(services::Error::Dropped(None))
    ));

    // The completed shutdown is the barrier proving the absences.
    bpa.shutdown().await;
    assert!(forwarded_rx.is_empty(), "nothing leaves the node");
    assert_eq!(
        storage.saves(),
        0,
        "nothing is stored, no report originated"
    );
}

/// A slot an Ingress Classifier writes is persisted with the record and read
/// by an Egress filter after the store checkpoint: the bundle parks for want
/// of a route, so the Egress hook can only see what the store kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ingress_slot_reaches_egress_across_the_store() {
    let mut pack = FilterPack::new("test");
    let slot = pack.annotation_slot::<u32>("mark", NonZeroUsize::new(16).unwrap());
    pack.ingress_classifier("writer", SlotWriter(slot.clone(), 7));
    let (seen_tx, seen_rx) = flume::unbounded();
    pack.egress_verifier(
        "reader",
        SlotReader {
            handle: slot.clone(),
            seen_tx,
        },
    );
    let (parked_tx, parked_rx) = flume::unbounded();
    let metadata = Arc::new(ObservedMetadata {
        inner: MetadataMemStorage::new(None),
        parked_tx,
    });
    let (bpa, cla, forwarded_rx) = setup(
        Bpa::builder()
            .add_filters(pack)
            .metadata_storage(metadata.clone()),
    )
    .await;

    // No peer serves ipn:0.4 yet.
    let id = arrive(&cla, SOURCE, BundleFlags::default(), "ipn:0.4.99").await;
    assert_eq!(next(&parked_rx).await, BundleStatus::Waiting);
    let stored = metadata
        .get(&id)
        .await
        .unwrap()
        .expect("the record is stored");
    assert_eq!(
        stored.metadata.slot(&slot),
        Some(7),
        "the slot is persisted"
    );

    // A route appears: the parked record re-enters from the store.
    cla.sink
        .get()
        .unwrap()
        .add_peer(
            cla::ClaAddress::Private("peer4".as_bytes().into()),
            &[node(4)],
        )
        .await
        .unwrap();
    assert_eq!(next(&seen_rx).await, Some(7), "Egress sees the stored slot");
    next(&forwarded_rx).await;

    bpa.shutdown().await;
}
