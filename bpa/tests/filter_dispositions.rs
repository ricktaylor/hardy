//! Filter hooks through the real pipeline: what the dispatcher does with each
//! filter outcome at the output hooks — the Deliver row of the disposition
//! table in the `filter` module docs (nothing at Egress drops a bundle) — and
//! the per-hop writes that follow the Egress Rewriters. Stored bytes that do
//! not decode are fatal, so the unit tests pin them: a panic inside a running
//! BPA aborts the test process.
//!
//! A correct Rewriter meets a call-time refusal — never the fail-stop — on a
//! bundle whose primary block forbids its edit: the flags are the sender's
//! choice, so an abort there would hand any peer a node-kill.

use core::num::{NonZeroU8, NonZeroU64};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use hardy_bpa::{
    Bytes, async_trait,
    bpa::{Bpa, BpaRegistration},
    builder::BpaBuilder,
    cla,
    filter::{RewriteContext, Rewriter, Verdict, Verifier, VerifyContext, pack::FilterPack},
    node_ids::NodeIds,
    services,
    storage::{
        BundleMemStorage, BundleStorage, MetadataMemStorage, MetadataStorage, RecoveryResponse,
        Result as StorageResult,
    },
    stream::{Receiver, Segment, Sender, buffer_stream},
};
// Aliased: the editor's error, beside the bpv7 `Error` imported here.
use hardy_bpv7::{
    Error,
    block::{Flags as BlockFlags, Type},
    builder::Builder,
    bundle::{Flags as BundleFlags, Id},
    crc::CrcType,
    creation_timestamp::CreationTimestamp,
    eid::{Eid, IpnNodeId, NodeId, Service},
    extension_editor::Error as EditorError,
    hop_info::HopInfo,
    parse::parse,
    status_report::{AdministrativeRecord, BundleStatusReport, ReasonCode},
};
use hardy_cbor::encode::emit;

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
    fn rewrite(&self, ctx: &mut RewriteContext<'_>) {
        let outcome = match ctx.editor().insert(
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
    fn verify(&self, ctx: &VerifyContext<'_>) -> Verdict {
        if ctx.bundle().primary.destination != self.destination {
            return Verdict::Continue(());
        }
        let _ = self.dropped_tx.send(());
        Verdict::Drop(self.reason)
    }
}

// Bundle storage the tests can see into: counts saves (a status report is
// saved before it is queued, so every report the node originates is one
// save).
struct ObservedStorage {
    inner: BundleMemStorage,
    saves: AtomicUsize,
}

impl ObservedStorage {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: BundleMemStorage::new(None, None),
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
        self.inner.save(data).await
    }

    async fn replace(&self, storage_name: &str, data: Bytes) -> StorageResult<()> {
        self.inner.replace(storage_name, data).await
    }

    async fn delete(&self, storage_name: &str) -> StorageResult<()> {
        self.inner.delete(storage_name).await
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

// A BPA with status reports on, observed storage, and a Deliver Verifier
// that drops the bundles addressed to `destination` with `reason`: (bpa,
// cla, forwarded, drops, storage, metadata).
async fn dropping_setup(
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
    pack.deliver_verifier("dropper", dropper);
    let storage = ObservedStorage::new();
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

/// Deliver `Drop(Some(reason))`: one deletion report carrying the filter's
/// reason; the bundle is not delivered and its record is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deliver_drop_with_a_reason_reports_once() {
    let (bpa, cla, forwarded_rx, _dropped_rx, storage, metadata) =
        dropping_setup(LOCAL, Some(ReasonCode::TrafficPared)).await;
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
    let (bpa, cla, forwarded_rx, dropped_rx, storage, metadata) = dropping_setup(LOCAL, None).await;
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

// Removes the Hop Count and Bundle Age blocks it is handed, recording each
// removal the editor accepted.
struct PerHopRemover {
    removed: Arc<Mutex<Vec<Type>>>,
}

impl Rewriter for PerHopRemover {
    fn rewrite(&self, ctx: &mut RewriteContext<'_>) {
        let targets: Vec<(u64, Type)> = ctx
            .bundle()
            .blocks
            .iter()
            .filter(|(_, b)| matches!(b.block_type, Type::HopCount | Type::BundleAge))
            .map(|(n, b)| (*n, b.block_type))
            .collect();
        for (block_number, block_type) in targets {
            if ctx.editor().remove(block_number).is_ok() {
                self.removed.lock().unwrap().push(block_type);
            }
        }
    }
}

/// The per-hop writes follow the Egress Rewriters and supersede their edits
/// to the blocks they write: a Rewriter removes a clockless bundle's Bundle
/// Age and its Hop Count, and the transmitted bundle carries both again, as
/// this hop writes them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_hop_writes_supersede_an_egress_rewriter() {
    let removed = Arc::new(Mutex::new(Vec::new()));
    let mut pack = FilterPack::new("test");
    pack.egress_rewriter(
        "remover",
        PerHopRemover {
            removed: removed.clone(),
        },
    );
    let (bpa, cla, forwarded_rx) = setup(Bpa::builder().add_filters(pack)).await;

    // No creation clock: the Bundle Age block is the bundle's only expiry
    // signal, and every forwarder must increase it.
    let (_, data) = Builder::new(SOURCE.parse().unwrap(), REMOTE.parse().unwrap())
        .with_hop_count(&HopInfo {
            limit: NonZeroU8::new(64).unwrap(),
            count: 3,
        })
        .add_extension_block(Type::BundleAge)
        .unwrap()
        .build(emit(&1000u64).0.into())
        .with_payload(b"payload".as_slice().into())
        .build(CreationTimestamp::from_parts(None, 1))
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

    let out = parse(next(&forwarded_rx).await).expect("the forwarded bundle parses");
    let removed = removed.lock().unwrap().clone();
    assert_eq!(removed.len(), 2, "the Rewriter removed two blocks");
    assert!(
        removed.contains(&Type::BundleAge) && removed.contains(&Type::HopCount),
        "the Rewriter's removals were accepted: {removed:?}"
    );
    let block = |block_type: Type| {
        out.bundle
            .blocks
            .values()
            .find(|b| b.block_type == block_type)
            .expect("the per-hop block is transmitted")
    };
    let hop_info = block(Type::HopCount)
        .extract::<HopInfo>(&out.data)
        .expect("the hop count decodes")
        .expect("the hop count is resident");
    assert_eq!(hop_info.count, 4, "this hop's increment");
    let age = block(Type::BundleAge)
        .extract::<u64>(&out.data)
        .expect("the bundle age decodes")
        .expect("the bundle age is resident");
    assert!(age >= 1000, "this hop's age, at least the received {age}");

    bpa.shutdown().await;
}
