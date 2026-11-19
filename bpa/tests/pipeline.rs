//! End-to-end bundle processing through the BPA, covering Suites A and B
//! of the component test plan (PLAN-BPA-01).

use core::{
    num::{NonZeroU8, NonZeroU32, NonZeroU64},
    time::Duration,
};
use hardy_bpa::{
    Bytes,
    async_trait,
    bpa::Bpa,
    cla::{self, Context, Factory, RegistrarExt, Request, factory_fn},
    filter::{
        Classifier, ClassifyContext, RewriteContext, Rewriter, Verdict, pack::FilterPack,
        slots::MetadataDelta,
    },
    node_ids::NodeIds,
    // `routing::Sink` stays qualified: the bare `Sink` here is the service
    // sink imported below.
    routing::{self, Agent, RegistrarExt as _, RouteAction, StaticAgent},
    service::{self, RegistrarExt as _, SendOptions, Service, Sink, StatusNotify, raw},
    storage::{BundleMemStorage, BundleStorage, MetadataMemStorage, MetadataStorage},
    stream::{Receiver, Segment, buffer_stream},
};
use hardy_bpv7::{
    block::{Flags as BlockFlags, Type},
    builder::Builder,
    bundle::{Flags, FragmentInfo, Id},
    crc::CrcType,
    creation_timestamp::CreationTimestamp,
    dtn_time::DtnTime,
    editor::{Chunk, Editor},
    // `eid::Service` is the service number inside an EID, not the service
    // trait registered here; it is aliased to the word the code already uses
    // for it.
    eid::{Eid, IpnNodeId, NodeId, Service as ServiceId},
    hop_info::HopInfo,
    parse::{Parsed, parse},
    reader::Availability,
    status_report::{AdministrativeRecord, BundleStatusReport, ReasonCode, StatusAssertion},
};
// Aliased: the key type, beside the block `Type` imported above.
#[cfg(feature = "rfc9173")]
use hardy_bpv7::bpsec::{
    encryptor::{Context as EncryptContext, Encryptor},
    key::{EncAlgorithm, Key, Operation, Type as KeyType},
};
use hardy_cbor::{
    decode::skip_value,
    encode::{Raw, emit, emit_array},
};
#[cfg(feature = "rfc9173")]
use rand::{TryRng, rngs::SysRng};
use std::{
    borrow::Cow,
    collections::VecDeque,
    env,
    slice::from_ref,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::available_parallelism,
};

struct PipelineCla {
    sink: Arc<dyn cla::Sink>,
    forwarded_tx: flume::Sender<Bytes>,
}

impl PipelineCla {
    fn factory() -> (impl Factory<Component = Self>, flume::Receiver<Bytes>) {
        let (tx, rx) = flume::bounded(16);
        (
            factory_fn(Request::default(), |cx: Context| {
                Ok(Arc::new(Self {
                    sink: cx.sink,
                    forwarded_tx: tx,
                }))
            }),
            rx,
        )
    }
}

#[async_trait]
impl cla::Cla for PipelineCla {
    async fn forward(
        &self,
        _lane: Option<u32>,
        _cla_addr: &cla::Address,
        _bundle_id: &Id,
        total_len: u64,
        stream: &mut dyn Receiver<cla::Segment>,
    ) -> cla::Result<cla::ForwardOutcome> {
        let bundle = buffer_stream(stream, total_len).await?;
        let _ = self.forwarded_tx.send(bundle);
        Ok(cla::ForwardOutcome::Sent)
    }
}

// ---------------------------------------------------------------------------
// A service that forwards each delivered bundle over a channel
// ---------------------------------------------------------------------------

struct TestService {
    sink: Arc<dyn Sink>,
    /// The endpoint the BPA settled on, and the source of every bundle
    /// this service originates.
    endpoint: Eid,
    received_tx: flume::Sender<(Eid, Bytes)>,
}

impl TestService {
    // `Context` and `factory_fn` name the CLA's here, so the service's
    // namesakes are qualified.
    fn factory() -> (
        impl service::Factory<Component = Self>,
        flume::Receiver<(Eid, Bytes)>,
    ) {
        let (tx, rx) = flume::bounded(16);
        (
            service::factory_fn(move |cx: service::Context| {
                Ok(Arc::new(Self {
                    sink: cx.sink,
                    endpoint: cx.endpoint,
                    received_tx: tx,
                }))
            }),
            rx,
        )
    }
}

#[async_trait]
impl Service for TestService {
    async fn deliver(
        &self,
        bundle_id: &Id,
        _expiry: time::OffsetDateTime,
        _ack_requested: bool,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> service::Result<()> {
        let payload = buffer_stream(stream, total_len).await?;
        self.received_tx
            .send((bundle_id.source.clone(), payload))
            .map_err(|e| service::Error::Internal(e.into()))
    }

    async fn report_status(
        &self,
        _bundle_id: &Id,
        _from: &Eid,
        _kind: StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<time::OffsetDateTime>,
    ) {
    }
}

// ---------------------------------------------------------------------------
// An echo service: swaps source and destination, and sends the bundle back
// ---------------------------------------------------------------------------

struct EchoService {
    sink: Arc<dyn raw::Sink>,
}

impl EchoService {
    fn factory() -> impl raw::Factory<Component = Self> {
        raw::factory_fn(|cx: raw::Context| Ok(Arc::new(Self { sink: cx.sink })))
    }
}

#[async_trait]
impl raw::Service for EchoService {
    async fn deliver(
        &self,
        _bundle_id: &Id,
        _expiry: time::OffsetDateTime,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> service::Result<()> {
        let data = buffer_stream(stream, total_len).await?;
        let Ok(Parsed {
            data, bundle: raw, ..
        }) = parse(data)
        else {
            return Ok(());
        };
        let Ok(editor) = Editor::new(&raw, &data).with_source(raw.primary.destination.clone())
        else {
            return Ok(());
        };
        let Ok(editor) = editor.with_destination(raw.primary.id.source.clone()) else {
            return Ok(());
        };
        let Ok(chunks) = editor.rebuild() else {
            return Ok(());
        };

        let mut reply = Chunk::flatten_bytes(chunks, data);
        self.sink.send(&mut reply).await.map(|_| ())
    }

    async fn report_status(
        &self,
        _bundle_id: &Id,
        _from: &Eid,
        _kind: StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<time::OffsetDateTime>,
    ) {
    }
}

// ---------------------------------------------------------------------------
// A CLA that records the arrival time inside `forward`, for the benchmarks
// ---------------------------------------------------------------------------

struct TimedCla {
    sink: Arc<dyn cla::Sink>,
    arrival_tx: flume::Sender<tokio::time::Instant>,
}

impl TimedCla {
    fn factory() -> (
        impl Factory<Component = Self>,
        flume::Receiver<tokio::time::Instant>,
    ) {
        let (tx, rx) = flume::bounded(4096);
        (
            factory_fn(Request::default(), move |cx: Context| {
                Ok(Arc::new(Self {
                    sink: cx.sink,
                    arrival_tx: tx,
                }))
            }),
            rx,
        )
    }
}

#[async_trait]
impl cla::Cla for TimedCla {
    async fn forward(
        &self,
        _lane: Option<u32>,
        _cla_addr: &cla::Address,
        _bundle_id: &Id,
        _total_len: u64,
        _stream: &mut dyn Receiver<cla::Segment>,
    ) -> cla::Result<cla::ForwardOutcome> {
        let _ = self.arrival_tx.send(tokio::time::Instant::now());
        Ok(cla::ForwardOutcome::Sent)
    }
}

// ---------------------------------------------------------------------------
// System information, printed alongside the benchmark results
// ---------------------------------------------------------------------------

fn print_system_info() {
    use std::fs;

    // CPU model
    if let Ok(cpuinfo) = fs::read_to_string("/proc/cpuinfo")
        && let Some(model) = cpuinfo
            .lines()
            .find(|l| l.starts_with("model name"))
            .and_then(|l| l.split(':').nth(1))
    {
        eprintln!("CPU: {}", model.trim());
    }

    // Logical cores
    let cores = available_parallelism().map(|n| n.get()).unwrap_or(0);
    eprintln!("Cores: {cores}");

    // Total RAM
    if let Ok(meminfo) = fs::read_to_string("/proc/meminfo")
        && let Some(total) = meminfo
            .lines()
            .find(|l| l.starts_with("MemTotal"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
    {
        eprintln!("RAM: {} GB", total / 1_048_576);
    }

    // OS
    if let Ok(release) = fs::read_to_string("/etc/os-release")
        && let Some(pretty) = release
            .lines()
            .find(|l| l.starts_with("PRETTY_NAME"))
            .and_then(|l| l.split('=').nth(1))
    {
        eprintln!("OS: {}", pretty.trim_matches('"'));
    }

    eprintln!("Arch: {}", env::consts::ARCH);

    // Tokio runtime config (from the #[tokio::test] attribute)
    let rt_metrics = tokio::runtime::Handle::current().metrics();
    eprintln!("Tokio workers: {}", rt_metrics.num_workers());
    eprintln!(
        "Profile: {}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    eprintln!(
        "Date: {}",
        time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap()
    );
}

// ---------------------------------------------------------------------------
// Bundle construction
// ---------------------------------------------------------------------------

fn build_bundle(source: &Eid, destination: &Eid, payload: &[u8]) -> Bytes {
    let (_, data) = Builder::new(source.clone(), destination.clone())
        .with_payload(Cow::Borrowed(payload))
        .build(CreationTimestamp::now())
        .expect("Failed to build bundle");
    Bytes::from(data)
}

// ---------------------------------------------------------------------------
// INT-BPA-01: Service-to-CLA Routing
// ---------------------------------------------------------------------------

/// A locally-originated bundle addressed to a remote node is forwarded
/// to the correct CLA peer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_to_cla_routing() {
    let bpa = Bpa::builder().build();
    bpa.start(false).await.unwrap();

    let (cla, forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();

    let peer_addr = cla::Address::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink.add_peer(peer_addr, &[remote_node]).await.unwrap();

    let (factory, _service_rx) = TestService::factory();
    let service = bpa
        .register_service(Some(ServiceId::Ipn(42)), factory)
        .await
        .unwrap();
    let source_eid = service.endpoint.clone();

    let dest: Eid = "ipn:0.2.99".parse().unwrap();
    service
        .sink
        .send(
            dest.clone(),
            Bytes::from_static(b"Hello remote"),
            Duration::from_secs(3600),
            None,
        )
        .await
        .unwrap();

    // Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for forwarded bundle")
    .expect("Channel closed");

    // Parse the forwarded bundle structurally, without keys.
    let Parsed {
        bundle: parsed_bundle,
        ..
    } = parse(forwarded).expect("Failed to parse forwarded bundle");

    assert_eq!(parsed_bundle.primary.id.source, source_eid);
    assert_eq!(parsed_bundle.primary.destination, dest);

    bpa.shutdown().await;
}

// A block ingress schedules for removal, here an unrecognised extension
// block flagged `delete_block_on_failure`, stays in the stored bundle, which
// is kept as received, and is stripped per attempt ahead of the Egress
// chain, so the transmitted wire form no longer carries it while the payload
// survives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_removal_applied_at_egress() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).build();
    bpa.start(false).await.unwrap();

    let (cla, forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();
    let peer_addr = cla::Address::Private("peer".as_bytes().into());
    let remote = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink.add_peer(peer_addr, &[remote]).await.unwrap();

    // A bundle to the remote node carrying an unrecognised block, type 999
    // and block number 2, flagged `delete_block_on_failure`, spliced between
    // the primary block and the payload.
    let source: Eid = "ipn:0.9.1".parse().unwrap();
    let dest: Eid = "ipn:0.2.99".parse().unwrap();
    let base = build_bundle(&source, &dest, b"payload");
    let unknown = emit_array(Some(5), |a| {
        a.emit(&999u64); // unrecognised block type
        a.emit(&2u64); // block number
        a.emit(&0x10u64); // flags: delete_block_on_failure
        a.emit(&0u64); // CRC type: none
        a.emit(&hardy_cbor::encode::Bytes(&[0xDE, 0xAD]));
    });
    assert_eq!(base[0], 0x9F, "bundle is an indefinite array");
    let (_, primary_len) = skip_value(&base[1..], 16).expect("skip primary");
    let insert = 1 + primary_len;
    let mut modified = Vec::with_capacity(base.len() + unknown.len());
    modified.extend_from_slice(&base[..insert]);
    modified.extend_from_slice(&unknown);
    modified.extend_from_slice(&base[insert..]);
    let inbound = Bytes::from(modified);

    // The bundle carries the unrecognised block as it arrives.
    let Parsed { bundle: pre, .. } = parse(inbound.clone()).expect("crafted bundle parses");
    assert!(
        pre.blocks
            .values()
            .any(|b| b.block_type == Type::Unrecognised(999)),
        "the unknown block is present as ingressed"
    );

    assert_eq!(
        cla.sink
            .dispatch(None, None, &mut inbound.clone())
            .await
            .unwrap(),
        cla::Acceptance::Accepted,
        "an unknown deletable block is accepted, not refused"
    );

    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("timeout waiting for the forwarded bundle")
    .expect("channel closed");

    let Parsed { bundle: fwd, .. } = parse(forwarded).expect("forwarded bundle parses");
    assert!(
        fwd.blocks
            .values()
            .all(|b| b.block_type != Type::Unrecognised(999)),
        "the deferred removal is applied at egress"
    );
    assert!(fwd.blocks.contains_key(&1), "the payload survives");

    bpa.shutdown().await;
}

// A raw service that captures each delivered bundle's wire bytes.
struct CapturingService {
    /// The endpoint the BPA settled on, which delivered bundles are
    /// addressed to.
    endpoint: Eid,
    delivered_tx: flume::Sender<Bytes>,
}

impl CapturingService {
    fn factory() -> (impl raw::Factory<Component = Self>, flume::Receiver<Bytes>) {
        let (delivered_tx, rx) = flume::unbounded();
        (
            raw::factory_fn(move |cx: raw::Context| {
                Ok(Arc::new(Self {
                    endpoint: cx.endpoint,
                    delivered_tx,
                }))
            }),
            rx,
        )
    }
}

#[async_trait]
impl raw::Service for CapturingService {
    async fn deliver(
        &self,
        _bundle_id: &Id,
        _expiry: time::OffsetDateTime,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> service::Result<()> {
        let data = buffer_stream(stream, total_len).await?;
        let _ = self.delivered_tx.send(data);
        Ok(())
    }

    async fn report_status(
        &self,
        _bundle_id: &Id,
        _from: &Eid,
        _kind: StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<time::OffsetDateTime>,
    ) {
    }
}

// The deliver-side counterpart of `deferred_removal_applied_at_egress`: a
// bundle addressed to a local raw service, carrying an unrecognised
// `delete_block_on_failure` block, is delivered with that block stripped,
// and the stored bundle stays as received.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_removal_applied_at_delivery() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).build();
    bpa.start(false).await.unwrap();

    let (factory, delivered_rx) = CapturingService::factory();
    let endpoint = bpa
        .register_raw_service(Some(ServiceId::Ipn(7)), factory)
        .await
        .unwrap()
        .endpoint
        .clone();

    let (cla, _forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();

    // A bundle to the local service, carrying an unrecognised block, type
    // 999 and block number 2, flagged `delete_block_on_failure`.
    let source: Eid = "ipn:0.9.1".parse().unwrap();
    let base = build_bundle(&source, &endpoint, b"payload");
    let unknown = emit_array(Some(5), |a| {
        a.emit(&999u64);
        a.emit(&2u64);
        a.emit(&0x10u64); // delete_block_on_failure
        a.emit(&0u64);
        a.emit(&hardy_cbor::encode::Bytes(&[0xDE, 0xAD]));
    });
    let (_, primary_len) = skip_value(&base[1..], 16).expect("skip primary");
    let insert = 1 + primary_len;
    let mut modified = Vec::with_capacity(base.len() + unknown.len());
    modified.extend_from_slice(&base[..insert]);
    modified.extend_from_slice(&unknown);
    modified.extend_from_slice(&base[insert..]);
    let inbound = Bytes::from(modified);

    assert_eq!(
        cla.sink
            .dispatch(None, None, &mut inbound.clone())
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    let delivered = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        delivered_rx.recv_async(),
    )
    .await
    .expect("timeout waiting for the delivered bundle")
    .expect("channel closed");

    let Parsed { bundle: del, .. } = parse(delivered).expect("delivered bundle parses");
    assert!(
        del.blocks
            .values()
            .all(|b| b.block_type != Type::Unrecognised(999)),
        "the deferred removal is applied at delivery"
    );
    assert!(del.blocks.contains_key(&1), "the payload survives");

    bpa.shutdown().await;
}

// Splices an unrecognised block, type 999 and block number 2, flagged
// `delete_block_on_failure`, between `base`'s primary block and its first
// canonical block, so that ingress schedules it for removal.
fn with_deletable_unknown(base: &Bytes) -> Bytes {
    let unknown = emit_array(Some(5), |a| {
        a.emit(&999u64); // unrecognised block type
        a.emit(&2u64); // block number
        a.emit(&0x10u64); // flags: delete_block_on_failure
        a.emit(&0u64); // CRC type: none
        a.emit(&hardy_cbor::encode::Bytes(&[0xDE, 0xAD]));
    });
    assert_eq!(base[0], 0x9F, "bundle is an indefinite array");
    let (_, primary_len) = skip_value(&base[1..], 16).expect("skip primary");
    let insert = 1 + primary_len;
    let mut modified = Vec::with_capacity(base.len() + unknown.len());
    modified.extend_from_slice(&base[..insert]);
    modified.extend_from_slice(&unknown);
    modified.extend_from_slice(&base[insert..]);
    Bytes::from(modified)
}

/// Records whether it was handed a block scheduled for removal, type 999,
/// then inserts a block of its own, which the editor numbers from the lowest
/// free block number, the removed block's when the removal came first.
struct ScheduledBlockObserver {
    saw_scheduled: Arc<AtomicBool>,
}

impl Rewriter for ScheduledBlockObserver {
    fn rewrite(&self, ctx: &mut RewriteContext<'_>) {
        if ctx
            .bundle()
            .blocks
            .values()
            .any(|b| b.block_type == Type::Unrecognised(999))
        {
            self.saw_scheduled.store(true, Ordering::SeqCst);
        }
        // A refusal is a no-match rather than a panic, a panicking filter
        // aborting the node, and `assert_insert_survives` catches a refused
        // insert.
        let _ = ctx.editor().insert(
            INSERTED,
            BlockFlags::default(),
            CrcType::None,
            emit(&42u64).0.into(),
        );
    }
}

// The bundle carries the Rewriter's insert and not the scheduled block.
fn assert_insert_survives(data: Bytes) {
    let out = parse(data).expect("the bundle parses");
    assert!(
        out.bundle
            .blocks
            .values()
            .all(|b| b.block_type != Type::Unrecognised(999)),
        "the scheduled removal is applied"
    );
    let inserted = out
        .bundle
        .blocks
        .values()
        .find(|b| b.block_type == INSERTED)
        .expect("the Rewriter's insert survives the removal");
    assert_eq!(inserted.payload(&out.data), Some(emit(&42u64).0.as_slice()));
}

/// The scheduled removals apply ahead of the Egress chain, so a Rewriter
/// never sees a block scheduled for removal and the block it inserts
/// travels. A removal applied afterwards could delete an insert that took
/// the removed block's number.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_removal_precedes_the_egress_rewriters() {
    let saw_scheduled = Arc::new(AtomicBool::new(false));
    let mut pack = FilterPack::new("test");
    pack.egress_rewriter(
        "observer",
        ScheduledBlockObserver {
            saw_scheduled: saw_scheduled.clone(),
        },
    );
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).add_filters(pack).build();
    bpa.start(false).await.unwrap();

    let (cla, forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();
    cla.sink
        .add_peer(
            cla::Address::Private("peer".as_bytes().into()),
            &[NodeId::Ipn(IpnNodeId {
                allocator_id: 0,
                node_number: 2,
            })],
        )
        .await
        .unwrap();

    let inbound = with_deletable_unknown(&build_bundle(
        &"ipn:0.9.1".parse().unwrap(),
        &"ipn:0.2.99".parse().unwrap(),
        b"payload",
    ));
    assert_eq!(
        cla.sink
            .dispatch(None, None, &mut inbound.clone())
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("timeout waiting for the forwarded bundle")
    .expect("channel closed");

    assert!(
        !saw_scheduled.load(Ordering::SeqCst),
        "the Rewriter never sees the scheduled block"
    );
    assert_insert_survives(forwarded);

    bpa.shutdown().await;
}

/// The Deliver counterpart: the scheduled removals apply ahead of the
/// Deliver chain, so a Deliver Rewriter never sees a block scheduled for
/// removal, and the raw service receives its insert.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_removal_precedes_the_deliver_chain() {
    let saw_scheduled = Arc::new(AtomicBool::new(false));
    let mut pack = FilterPack::new("test");
    pack.deliver_rewriter(
        "observer",
        ScheduledBlockObserver {
            saw_scheduled: saw_scheduled.clone(),
        },
    );
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).add_filters(pack).build();
    bpa.start(false).await.unwrap();

    let (factory, delivered_rx) = CapturingService::factory();
    let endpoint = bpa
        .register_raw_service(Some(ServiceId::Ipn(7)), factory)
        .await
        .unwrap()
        .endpoint
        .clone();
    let (cla, _forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();

    let inbound = with_deletable_unknown(&build_bundle(
        &"ipn:0.9.1".parse().unwrap(),
        &endpoint,
        b"payload",
    ));
    assert_eq!(
        cla.sink
            .dispatch(None, None, &mut inbound.clone())
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // Event-driven wait; the timeout only bounds a regression.
    let delivered = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        delivered_rx.recv_async(),
    )
    .await
    .expect("timeout waiting for the delivered bundle")
    .expect("channel closed");

    assert!(
        !saw_scheduled.load(Ordering::SeqCst),
        "the Rewriter never sees the scheduled block"
    );
    assert_insert_survives(delivered);

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// INT-BPA-02: Echo Round-Trip
// ---------------------------------------------------------------------------

/// A bundle dispatched via CLA to the echo service is reflected back
/// and forwarded out via the CLA with source/destination swapped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn echo_round_trip() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();

    let bpa = Bpa::builder().node_ids(node_ids).build();
    bpa.start(false).await.unwrap();

    bpa.register_raw_service(Some(ServiceId::Ipn(7)), EchoService::factory())
        .await
        .unwrap();

    let (cla, forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();

    let peer_addr = cla::Address::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    // Inbound from the remote node, addressed to the echo service.
    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let echo_dest: Eid = "ipn:0.1.7".parse().unwrap();
    let mut inbound = build_bundle(&remote_source, &echo_dest, b"ping");

    assert_eq!(
        cla.sink
            .dispatch(Some(&remote_node), None, &mut inbound)
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // The echo service sends the bundle back, from ipn:0.1.7 to ipn:0.2.1,
    // which the BPA routes to the CLA peer. Event-driven wait; the timeout
    // only bounds a regression.
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for echo reply")
    .expect("Channel closed");

    // Parse the echo reply structurally, without keys.
    let Parsed {
        bundle: parsed_bundle,
        ..
    } = parse(forwarded).expect("Failed to parse echo reply");

    assert_eq!(
        parsed_bundle.primary.destination, remote_source,
        "Echo reply destination should be the original source"
    );
    assert_eq!(
        parsed_bundle.primary.id.source, echo_dest,
        "Echo reply source should be the echo service"
    );

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// INT-BPA-12: Streamed service originate
// ---------------------------------------------------------------------------

// Registers a service and a CLA peer. The returned BPA, sink holder,
// forwarded channel and service EID drive `ServiceSink::send` directly with
// multi-segment streams.
async fn streamed_originate_setup() -> (Bpa, Arc<EchoService>, flume::Receiver<Bytes>, Eid) {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();

    let bpa = Bpa::builder().node_ids(node_ids).build();
    bpa.start(false).await.unwrap();

    // The service only captures its sink, which the test drives itself.
    let svc = bpa
        .register_raw_service(Some(ServiceId::Ipn(7)), EchoService::factory())
        .await
        .unwrap();

    let (cla, forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();

    let peer_addr = cla::Address::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    (bpa, svc, forwarded_rx, "ipn:0.1.7".parse().unwrap())
}

/// A bundle streamed through `ServiceSink::send` in several segments
/// originates as a whole-buffer `send` does: the returned id matches the
/// built bundle, which is forwarded to the CLA peer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_streamed_originate_forwards() {
    let (bpa, svc, forwarded_rx, source_eid) = streamed_originate_setup().await;

    let dest: Eid = "ipn:0.2.1".parse().unwrap();
    let data = build_bundle(&source_eid, &dest, b"streamed-originate");

    // Two `Next` segments and a `Final`. The channel holds all three, so the
    // producer completes before the sink pulls.
    let third = data.len() / 3;
    let (tx, mut rx) = hardy_async::channel::bounded(4);
    tx.send(Segment::Next(data.slice(..third))).await.unwrap();
    tx.send(Segment::Next(data.slice(third..2 * third)))
        .await
        .unwrap();
    tx.send(Segment::Final(data.slice(2 * third..)))
        .await
        .unwrap();
    drop(tx);

    let id = svc.sink.send(&mut rx).await.expect("streamed send failed");
    assert_eq!(id.source, source_eid);

    // Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for forwarded bundle")
    .expect("Channel closed");

    let parsed = parse(forwarded).expect("Failed to parse forwarded bundle");
    assert_eq!(parsed.bundle.primary.id, id);
    assert_eq!(parsed.bundle.primary.destination, dest);

    bpa.shutdown().await;
}

/// Dropping the producer before `Final` cancels the send: the caller
/// receives `StreamCancelled`, nothing enters custody and nothing is
/// forwarded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_streamed_cancel_stores_nothing() {
    let (bpa, svc, forwarded_rx, source_eid) = streamed_originate_setup().await;

    let dest: Eid = "ipn:0.2.1".parse().unwrap();
    let data = build_bundle(&source_eid, &dest, b"cancelled");

    let (tx, mut rx) = hardy_async::channel::bounded(4);
    tx.send(Segment::Next(data.slice(..data.len() / 2)))
        .await
        .unwrap();
    drop(tx); // The producer aborts, without a `Final`.

    let result = svc.sink.send(&mut rx).await;
    assert!(matches!(
        result,
        Err(hardy_bpa::service::Error::StreamCancelled)
    ));

    // Nothing was originated, so no forward arrives.
    assert!(
        // Event-driven wait; the timeout only bounds a regression.
        tokio::time::timeout(
            tokio::time::Duration::from_millis(500),
            forwarded_rx.recv_async(),
        )
        .await
        .is_err(),
        "cancelled stream must not originate a bundle"
    );

    bpa.shutdown().await;
}

/// Unregistering a service wakes its in-flight sends at once: a consumer
/// parked behind a stalled producer fails with `StreamCancelled` as the
/// registration ends, without awaiting the producer's next segment.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_unregister_cancels_parked_send() {
    let (bpa, svc, _forwarded_rx, source_eid) = streamed_originate_setup().await;

    let dest: Eid = "ipn:0.2.1".parse().unwrap();
    let data = build_bundle(&source_eid, &dest, b"parked");

    // One segment, then a stall. The sender stays alive throughout, so only
    // registration teardown ends the stream.
    let (tx, mut rx) = hardy_async::channel::bounded(2);
    hardy_async::channel::Sender::send(&tx, Segment::Next(data.slice(..data.len() / 2)))
        .await
        .unwrap();

    let parked = {
        let svc = svc.clone();
        tokio::spawn(async move { svc.sink.send(&mut rx).await })
    };

    // Let the consumer enter the stream and park on the second pull.
    // Known test-guide deviation (timed quiesce): scheduled for the
    // dedicated pipeline de-flake pass (see bpa/docs/TODO.md).
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    svc.sink.unregister().await;

    let result = tokio::time::timeout(tokio::time::Duration::from_secs(5), parked)
        .await
        .expect("parked send was not woken by unregistration")
        .expect("task panicked");
    assert!(matches!(
        result,
        Err(hardy_bpa::service::Error::StreamCancelled)
    ));
    drop(tx);

    bpa.shutdown().await;
}

/// The source-spoof check holds on the streamed path, rejecting a bundle
/// whose source is not the registered service endpoint.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_streamed_send_rejects_spoofed_source() {
    let (bpa, svc, _forwarded_rx, _source_eid) = streamed_originate_setup().await;

    let spoofed: Eid = "ipn:0.1.99".parse().unwrap();
    let dest: Eid = "ipn:0.2.1".parse().unwrap();
    let data = build_bundle(&spoofed, &dest, b"spoofed");

    let (tx, mut rx) = hardy_async::channel::bounded(1);
    tx.send(Segment::Final(data)).await.unwrap();
    drop(tx);

    let result = svc.sink.send(&mut rx).await;
    assert!(matches!(
        result,
        Err(hardy_bpa::service::Error::InvalidDestination(_))
    ));

    bpa.shutdown().await;
}

// Sends `data` through the raw service sink and returns the bundle the CLA
// forwards, which origination admitted.
#[cfg(feature = "rfc9173")]
async fn originate_raw_and_forward(
    svc: &EchoService,
    forwarded_rx: &flume::Receiver<Bytes>,
    data: Bytes,
) -> Parsed {
    let (tx, mut rx) = hardy_async::channel::bounded(1);
    tx.send(Segment::Final(data)).await.unwrap();
    drop(tx);
    svc.sink
        .send(&mut rx)
        .await
        .expect("origination admits the bundle");
    // Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("timeout waiting for the forwarded bundle")
    .expect("channel closed");
    parse(forwarded).expect("the forwarded bundle parses")
}

/// Origination admits a Hop Count block this node cannot decrypt, as
/// ingress does, its increment being a SHOULD, and the bundle leaves with
/// the block unchanged.
#[cfg(feature = "rfc9173")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_streamed_send_accepts_an_undecryptable_hop_count() {
    let (bpa, svc, forwarded_rx, source_eid) = streamed_originate_setup().await;

    // Immaterial key value: generated per the no-literal-keys rule.
    let mut key_bytes = vec![0u8; 32];
    SysRng.try_fill_bytes(&mut key_bytes).unwrap();
    let key = Key {
        key_type: KeyType::octet_sequence(key_bytes),
        key_algorithm: None,
        enc_algorithm: Some(EncAlgorithm::A256GCM),
        operations: Some([Operation::Encrypt].into_iter().collect()),
        id: None,
        key_use: None,
    };
    let (built, data) = Builder::new(source_eid.clone(), "ipn:0.2.1".parse().unwrap())
        .with_hop_count(&HopInfo {
            limit: NonZeroU8::new(64).unwrap(),
            count: 0,
        })
        .with_payload(b"undecryptable hop count".as_slice().into())
        .build(CreationTimestamp::now())
        .unwrap();
    let hop_count = *built
        .blocks
        .iter()
        .find(|(_, b)| b.block_type == Type::HopCount)
        .expect("the builder emitted the Hop Count block")
        .0;
    let encrypted = parse(Bytes::from(
        Encryptor::new(&built, &data)
            .encrypt_block(
                hop_count,
                EncryptContext::AES_GCM(Default::default()),
                source_eid,
                &key,
            )
            .map_err(|(_, e)| e)
            .expect("encrypt the Hop Count block")
            .rebuild()
            .expect("rebuild the encrypted bundle"),
    ))
    .unwrap();

    let out = originate_raw_and_forward(&svc, &forwarded_rx, encrypted.data.clone()).await;
    let before = &encrypted.bundle.blocks[&hop_count];
    let after = out
        .bundle
        .blocks
        .values()
        .find(|b| b.block_type == Type::HopCount)
        .expect("the Hop Count block travels");
    assert!(after.bcb.is_some(), "the block stays encrypted");
    assert_eq!(
        after.payload(&out.data),
        before.payload(&encrypted.data),
        "the encrypted block travels unchanged"
    );

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// INT-BPA-03: Local Delivery
// ---------------------------------------------------------------------------

/// A bundle dispatched by a CLA and addressed to a local service reaches
/// that service's `deliver`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_delivery() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();

    let bpa = Bpa::builder().node_ids(node_ids).build();
    bpa.start(false).await.unwrap();

    let (factory, service_rx) = TestService::factory();
    bpa.register_service(Some(ServiceId::Ipn(42)), factory)
        .await
        .unwrap();

    let (cla, _forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let local_dest: Eid = "ipn:0.1.42".parse().unwrap();
    let mut inbound = build_bundle(&remote_source, &local_dest, b"Hello local");

    assert_eq!(
        cla.sink.dispatch(None, None, &mut inbound).await.unwrap(),
        cla::Acceptance::Accepted
    );

    let (source, payload) =
        // Event-driven wait; the timeout only bounds a regression.
        tokio::time::timeout(tokio::time::Duration::from_secs(5), service_rx.recv_async())
            .await
            .expect("Timeout waiting for local delivery")
            .expect("Channel closed");

    assert_eq!(source, remote_source, "Delivered source should match");
    assert_eq!(
        payload.as_ref(),
        b"Hello local",
        "Delivered payload should match"
    );

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// Reception report reason codes (RFC 9172 Section 7.1)
// ---------------------------------------------------------------------------

// Splices a BCB with an unrecognised security context, id 99, targeting the
// payload into `data` as block number 2, flagged must-replicate, which a
// payload target requires, and report-on-failure.
fn splice_unrecognised_bcb(data: &[u8]) -> Bytes {
    // The ASB as a CBOR sequence: targets [1], context id 99, context flags
    // 0 for no parameters, the source EID, then one result list per target.
    let result_val = [0x41u8, 0xAA]; // result value: bytes(0xAA)
    let source: Eid = "ipn:0.2.1".parse().unwrap();
    let mut asb = emit(&[1u64]).0;
    asb.extend(emit(&99u64).0);
    asb.extend(emit(&0u64).0);
    asb.extend(emit(&source).0);
    asb.extend(emit_array(Some(1), |results| {
        results.emit_array(Some(1), |target_results| {
            target_results.emit(&(1u64, Raw(&result_val)));
        });
    }));

    let bcb_block = emit_array(Some(5), |a| {
        a.emit(&12u64); // block type: BCB
        a.emit(&2u64); // block number
        a.emit(&0x03u64); // flags: must_replicate | report_on_failure
        a.emit(&0u64); // CRC type: none
        a.emit(&hardy_cbor::encode::Bytes(&asb));
    });

    assert_eq!(data[0], 0x9F, "Bundle should start with indefinite array");
    let (_, primary_len) = skip_value(&data[1..], 16).expect("Should skip primary block");
    let insert_pos = 1 + primary_len;
    let mut modified = Vec::with_capacity(data.len() + bcb_block.len());
    modified.extend_from_slice(&data[..insert_pos]);
    modified.extend_from_slice(&bcb_block);
    modified.extend_from_slice(&data[insert_pos..]);
    modified.into()
}

// A transit bundle carrying a BCB this node cannot understand, of an
// unrecognised security context and with `report_on_failure` set, is still
// forwarded, and the requested reception report cites the RFC 9172
// `UnknownSecurityOperation` reason rather than the generic
// `BlockUnsupported`. The bundle is addressed to a remote node so that it
// forwards, a payload-targeting BCB being decrypted only at delivery.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reception_report_carries_unknown_security_operation() {
    use hardy_bpv7::status_report::{AdministrativeRecord, ReasonCode};

    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();

    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .status_reports(true)
        .build();
    bpa.start(false).await.unwrap();

    // A peer for the remote node ipn:0.2, the route for both the forwarded
    // bundle and the reception report, report-to defaulting to the source.
    let (cla, forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();
    let peer_addr = cla::Address::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    // Inbound transit bundle requesting a reception report.
    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let dest: Eid = "ipn:0.2.99".parse().unwrap();
    let (_, data) = Builder::new(remote_source.clone(), dest.clone())
        .with_flags(Flags {
            receipt_report_requested: true,
            ..Default::default()
        })
        .with_payload(Cow::Borrowed(b"opaque"))
        .build(CreationTimestamp::now())
        .unwrap();
    let mut inbound = splice_unrecognised_bcb(&data);

    assert_eq!(
        cla.sink
            .dispatch(Some(&remote_node), None, &mut inbound)
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // Two bundles come back out of the CLA in either order: the reception
    // report (to ipn:0.2.1) and the forwarded original (to ipn:0.2.99).
    let mut report = None;
    let mut original = None;
    for _ in 0..2 {
        // Event-driven wait; the timeout only bounds a regression.
        let forwarded = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            forwarded_rx.recv_async(),
        )
        .await
        .expect("Timeout waiting for forwarded bundle")
        .expect("Channel closed");
        let parsed = parse(forwarded).expect("Failed to parse forwarded");
        if parsed.bundle.primary.flags.is_admin_record {
            report = Some(parsed);
        } else {
            original = Some(parsed);
        }
    }

    // The original is forwarded intact, BCB and all, an operation this node
    // cannot understand being left to a downstream security acceptor.
    let original = original.expect("Original bundle should be forwarded");
    assert_eq!(original.bundle.primary.destination, dest);
    assert!(
        original
            .bundle
            .blocks
            .values()
            .any(|b| matches!(b.block_type, Type::BlockSecurity)),
        "Forwarded bundle should still carry the BCB"
    );

    // The reception report goes to the source and carries the RFC 9172 code.
    let report = report.expect("Reception report should be emitted");
    assert_eq!(report.bundle.primary.destination, remote_source);
    let body = report
        .bundle
        .blocks
        .get(&1)
        .expect("Report has a payload block")
        .payload(&report.data)
        .expect("Report payload in bundle");
    let record = hardy_cbor::decode::parse::<AdministrativeRecord>(body)
        .expect("Report payload is an administrative record");
    let AdministrativeRecord::BundleStatusReport(status) = record;
    assert_eq!(status.reason, ReasonCode::UnknownSecurityOperation);
    assert!(status.received.is_some(), "Reception assertion present");
    assert_eq!(status.bundle_id.source, remote_source);

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// INT-BPA-12: Streamed CLA ingress
// ---------------------------------------------------------------------------

/// A bundle dispatched through `Sink::dispatch` in several segments
/// ingresses as the whole-buffer `dispatch` does, the reassembled bundle
/// reaching its local service.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cla_streamed_ingress_delivers() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();

    let bpa = Bpa::builder().node_ids(node_ids).build();
    bpa.start(false).await.unwrap();

    let (factory, service_rx) = TestService::factory();
    bpa.register_service(Some(ServiceId::Ipn(42)), factory)
        .await
        .unwrap();

    let (cla, _forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let local_dest: Eid = "ipn:0.1.42".parse().unwrap();
    let inbound = build_bundle(&remote_source, &local_dest, b"streamed ingress");

    // Three segments through a bounded channel with a spawned producer, so
    // the pull side drives.
    let third = inbound.len() / 3;
    let (tx, mut rx) = hardy_async::channel::bounded(1);
    let segments = vec![
        Segment::Next(inbound.slice(..third)),
        Segment::Next(inbound.slice(third..2 * third)),
        Segment::Final(inbound.slice(2 * third..)),
    ];
    let producer = tokio::spawn(async move {
        for segment in segments {
            hardy_async::channel::Sender::send(&tx, segment)
                .await
                .unwrap();
        }
    });

    assert_eq!(
        cla.sink.dispatch(None, None, &mut rx).await.unwrap(),
        cla::Acceptance::Accepted
    );
    producer.await.unwrap();

    let (source, payload) =
        // Event-driven wait; the timeout only bounds a regression.
        tokio::time::timeout(tokio::time::Duration::from_secs(5), service_rx.recv_async())
            .await
            .expect("Timeout waiting for streamed delivery")
            .expect("Channel closed");
    assert_eq!(source, remote_source);
    assert_eq!(payload.as_ref(), b"streamed ingress");

    bpa.shutdown().await;
}

/// Unregistering a CLA wakes its in-flight streams at once: a consumer
/// parked behind a stalled producer fails with `StreamCancelled` as the
/// registration ends, without awaiting the producer's next segment.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cla_unregister_cancels_parked_stream() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();

    let bpa = Bpa::builder().node_ids(node_ids).build();
    bpa.start(false).await.unwrap();

    let (cla, _forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();

    // A producer that sends one segment then stalls. The sender stays alive
    // throughout, so only registration teardown ends the stream. The segment
    // is a real bundle prefix: the header pass parses eagerly, so arbitrary
    // bytes would be rejected structurally rather than leaving the consumer
    // parked on the next pull.
    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let local_dest: Eid = "ipn:0.1.42".parse().unwrap();
    let inbound = build_bundle(&remote_source, &local_dest, b"parked");
    let (tx, mut rx) = hardy_async::channel::bounded(2);
    hardy_async::channel::Sender::send(&tx, Segment::Next(inbound.slice(..inbound.len() / 2)))
        .await
        .unwrap();

    let parked = {
        let cla = cla.clone();
        tokio::spawn(async move { cla.sink.dispatch(None, None, &mut rx).await })
    };

    // Let the consumer enter the stream and park on the second pull.
    // Known test-guide deviation (timed quiesce): scheduled for the
    // dedicated pipeline de-flake pass (see bpa/docs/TODO.md).
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    cla.sink.unregister().await;

    // The parked pull fails promptly. The assertion is event-driven and the
    // timeout only bounds a regression. The teardown is reported as the dead
    // registration rather than as a per-bundle refusal.
    let result = tokio::time::timeout(tokio::time::Duration::from_secs(5), parked)
        .await
        .expect("parked stream was not woken by unregistration")
        .expect("task panicked");
    assert!(matches!(result, Err(hardy_bpa::cla::Error::Disconnected)));
    drop(tx);

    bpa.shutdown().await;
}

/// A producer that dies before `Final` has truncated the bundle: the sink
/// refuses it, so a CLA withholds its transfer ack, and nothing is
/// delivered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cla_streamed_ingress_truncation_is_refused() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();

    let bpa = Bpa::builder().node_ids(node_ids).build();
    bpa.start(false).await.unwrap();

    let (factory, service_rx) = TestService::factory();
    bpa.register_service(Some(ServiceId::Ipn(42)), factory)
        .await
        .unwrap();

    let (cla, _forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let local_dest: Eid = "ipn:0.1.42".parse().unwrap();
    let inbound = build_bundle(&remote_source, &local_dest, b"truncated");

    let (tx, mut rx) = hardy_async::channel::bounded(4);
    hardy_async::channel::Sender::send(&tx, Segment::Next(inbound.slice(..inbound.len() / 2)))
        .await
        .unwrap();
    drop(tx); // no Final

    let result = cla.sink.dispatch(None, None, &mut rx).await;
    assert!(matches!(result, Ok(cla::Acceptance::Refused)));

    // Known test-guide deviation (quiet-window absence assert):
    // scheduled for the dedicated pipeline de-flake pass (see bpa/docs/TODO.md).
    assert!(
        tokio::time::timeout(
            tokio::time::Duration::from_millis(500),
            service_rx.recv_async()
        )
        .await
        .is_err(),
        "truncated stream must not deliver"
    );

    bpa.shutdown().await;
}

// A node with a CLA and a service on ipn:0.1.42, for the abandoned-transfer
// tests. Returns the BPA, the service's delivery receiver and the CLA.
async fn abandoned_transfer_setup() -> (
    Bpa,
    Arc<TestService>,
    flume::Receiver<(Eid, Bytes)>,
    Arc<PipelineCla>,
) {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();

    let bpa = Bpa::builder().node_ids(node_ids).build();
    bpa.start(false).await.unwrap();

    let (factory, service_rx) = TestService::factory();
    let service = bpa
        .register_service(Some(ServiceId::Ipn(42)), factory)
        .await
        .unwrap();

    let (cla, _forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();

    (bpa, service, service_rx, cla)
}

// Sends `segments`, none of them a `Final`, drops the producer, and returns
// the sink's verdict.
async fn dispatch_abandoned(cla: &PipelineCla, segments: Vec<Segment>) -> cla::Acceptance {
    let (tx, mut rx) = hardy_async::channel::bounded(segments.len());
    for segment in segments {
        hardy_async::channel::Sender::send(&tx, segment)
            .await
            .unwrap();
    }
    drop(tx); // no Final

    cla.sink.dispatch(None, None, &mut rx).await.unwrap()
}

/// A whole bundle sent in `Segment::Next` whose producer then dies before
/// `Final` is an abandoned transfer: the sink refuses it, so a CLA withholds
/// its transfer ack, and nothing is delivered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cla_complete_at_head_without_final_is_refused() {
    let (bpa, _app, service_rx, cla) = abandoned_transfer_setup().await;

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let local_dest: Eid = "ipn:0.1.42".parse().unwrap();
    let inbound = build_bundle(&remote_source, &local_dest, b"abandoned");

    assert_eq!(
        dispatch_abandoned(&cla, vec![Segment::Next(inbound)]).await,
        cla::Acceptance::Refused
    );

    // The completed shutdown is the barrier proving nothing was delivered.
    bpa.shutdown().await;
    assert!(
        service_rx.is_empty(),
        "an abandoned transfer must not deliver"
    );
}

/// The same for a bundle segmented past its outer break: the producer dies
/// after the last byte but before `Final`, and the sink refuses the
/// transfer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cla_segmented_without_final_is_refused() {
    let (bpa, _app, service_rx, cla) = abandoned_transfer_setup().await;

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let local_dest: Eid = "ipn:0.1.42".parse().unwrap();
    let inbound = build_bundle(&remote_source, &local_dest, &vec![0xA5_u8; 20_000]);

    // Every byte arrives, in 1000-byte `Next` segments, which forces the
    // `Partial` route.
    let segments = inbound
        .chunks(1000)
        .map(|chunk| Segment::Next(Bytes::copy_from_slice(chunk)))
        .collect();
    assert_eq!(
        dispatch_abandoned(&cla, segments).await,
        cla::Acceptance::Refused
    );

    // The completed shutdown is the barrier proving nothing was delivered.
    bpa.shutdown().await;
    assert!(
        service_rx.is_empty(),
        "an abandoned transfer must not deliver"
    );
}

// ---------------------------------------------------------------------------
// Streamed ingress: a CLA delivers a bundle split across many segments,
// its payload arriving after its header, which exercises the `Partial`
// route end to end, the payload drained by the `ValidatingReceiver`.
// ---------------------------------------------------------------------------

// A `Receiver` that yields a fixed sequence of segments and then reports
// the producer gone, as a CLA reassembling a transfer into segments does.
struct SegmentReceiver {
    segments: Mutex<VecDeque<cla::Segment>>,
}

impl SegmentReceiver {
    // `data` in `chunk`-byte segments, `Next` for all but the last, which
    // is `Final`.
    fn new(data: &[u8], chunk: usize) -> Self {
        let mut segments = VecDeque::new();
        let mut off = 0;
        while off < data.len() {
            let end = (off + chunk).min(data.len());
            let piece = Bytes::copy_from_slice(&data[off..end]);
            segments.push_back(if end == data.len() {
                cla::Segment::Final(piece)
            } else {
                cla::Segment::Next(piece)
            });
            off = end;
        }
        Self {
            segments: Mutex::new(segments),
        }
    }

    // Segments the parser has not pulled. Non-zero after a dispatch means
    // the payload tail was never drained.
    fn remaining(&self) -> usize {
        self.segments.lock().unwrap().len()
    }
}

#[async_trait]
impl Receiver<cla::Segment> for SegmentReceiver {
    async fn recv(&mut self) -> Result<cla::Segment, hardy_bpa::stream::RecvError> {
        self.segments
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(hardy_bpa::stream::RecvError)
    }
}

// A segment source that yields a fixed prefix of a bundle's segments and
// then parks forever, a transfer whose producer never completes. A consumer
// that returns while this source is parked provably did not await the
// payload.
struct ParkedReceiver {
    segments: VecDeque<cla::Segment>,
}

impl ParkedReceiver {
    // The first `count` `chunk`-byte segments of `data`, all `Next`, so the
    // stream never completes.
    fn new(data: &[u8], chunk: usize, count: usize) -> Self {
        Self {
            segments: data
                .chunks(chunk)
                .take(count)
                .map(|c| cla::Segment::Next(Bytes::copy_from_slice(c)))
                .collect(),
        }
    }
}

#[async_trait]
impl Receiver<cla::Segment> for ParkedReceiver {
    async fn recv(&mut self) -> Result<cla::Segment, hardy_bpa::stream::RecvError> {
        match self.segments.pop_front() {
            Some(seg) => Ok(seg),
            None => futures::future::pending().await,
        }
    }
}

// An inbound bundle with a 20 KB payload, delivered in 1000-byte segments, is
// reassembled and delivered intact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn segmented_payload_local_delivery() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).build();
    bpa.start(false).await.unwrap();

    let (factory, service_rx) = TestService::factory();
    bpa.register_service(Some(ServiceId::Ipn(42)), factory)
        .await
        .unwrap();

    let (cla, _forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let local_dest: Eid = "ipn:0.1.42".parse().unwrap();
    let payload = vec![0xA5_u8; 20_000];
    let inbound = build_bundle(&remote_source, &local_dest, &payload);

    // The bundle as a stream of 1000-byte segments, which forces the
    // `Partial` route.
    let mut stream = SegmentReceiver::new(&inbound, 1000);
    assert_eq!(
        cla.sink.dispatch(None, None, &mut stream).await.unwrap(),
        cla::Acceptance::Accepted
    );

    let (source, delivered) =
        // Event-driven wait; the timeout only bounds a regression.
        tokio::time::timeout(tokio::time::Duration::from_secs(5), service_rx.recv_async())
            .await
            .expect("Timeout waiting for streamed local delivery")
            .expect("Channel closed");
    assert_eq!(source, remote_source, "Delivered source should match");
    assert_eq!(
        delivered.as_ref(),
        payload.as_slice(),
        "Full streamed payload should be delivered intact"
    );

    bpa.shutdown().await;
}

// A replayed arrival is settled before the drain: the first copy committed,
// and was tombstoned once delivered, so the replay's duplicate probe fires
// while its payload source is still parked, and the transfer is acked and
// dropped without spooling a byte. A consumer that returns while the source
// is parked provably did not await the payload.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_arrival_never_awaits_the_payload() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).build();
    bpa.start(false).await.unwrap();

    let (factory, service_rx) = TestService::factory();
    bpa.register_service(Some(ServiceId::Ipn(42)), factory)
        .await
        .unwrap();

    let (cla, _forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let local_dest: Eid = "ipn:0.1.42".parse().unwrap();
    let payload = vec![0xA5_u8; 20_000];
    let inbound = build_bundle(&remote_source, &local_dest, &payload);

    // The first copy is delivered whole, so the record commits and its id
    // enters the recently-committed cache. Delivery tombstones the record,
    // but the probe answers from the cached id rather than from the
    // record's state.
    let mut stream = SegmentReceiver::new(&inbound, 1000);
    assert_eq!(
        cla.sink.dispatch(None, None, &mut stream).await.unwrap(),
        cla::Acceptance::Accepted
    );
    // Event-driven wait; the timeout only bounds a regression.
    tokio::time::timeout(tokio::time::Duration::from_secs(5), service_rx.recv_async())
        .await
        .expect("Timeout waiting for first delivery")
        .expect("Channel closed");

    // The replay has resident headers and a payload parked forever. The
    // duplicate probe settles the transfer, acked and dropped, without the
    // payload. The timeout only bounds a regression, since a pipeline that
    // awaited the payload would park forever.
    let mut replay = ParkedReceiver::new(&inbound, 1000, 3);
    let acceptance = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        cla.sink.dispatch(None, None, &mut replay),
    )
    .await
    .expect("Timeout: the duplicate arrival awaited its parked payload")
    .unwrap();
    assert_eq!(
        acceptance,
        cla::Acceptance::Accepted,
        "a committed duplicate is acked so the peer stops resending"
    );

    // The completed shutdown is the barrier proving the absence.
    bpa.shutdown().await;
    assert!(service_rx.is_empty(), "the duplicate must not deliver");
}

// A bundle whose creation time plus lifetime is already past, expired on
// arrival. Sent in segments, its payload streams as `Partial`.
fn build_expired_bundle(source: &Eid, destination: &Eid, payload: &[u8]) -> Bytes {
    let past = time::OffsetDateTime::now_utc() - time::Duration::hours(1);
    let timestamp = CreationTimestamp::from_parts(Some(DtnTime::saturating_from(past)), 1);
    let (_, data) = Builder::new(source.clone(), destination.clone())
        .with_lifetime(Duration::from_secs(60))
        .with_payload(Cow::Borrowed(payload))
        .build(timestamp)
        .expect("Failed to build expired bundle");
    Bytes::from(data)
}

// A bundle carrying a Hop Count block whose count already exceeds its limit.
// Sent in segments, its payload streams as `Partial`.
fn build_hop_exhausted_bundle(source: &Eid, destination: &Eid, payload: &[u8]) -> Bytes {
    let hop = HopInfo {
        limit: NonZeroU8::new(1).unwrap(),
        count: 2,
    };
    let (_, data) = Builder::new(source.clone(), destination.clone())
        .with_hop_count(&hop)
        .with_payload(Cow::Borrowed(payload))
        .build(CreationTimestamp::now())
        .expect("Failed to build hop-exhausted bundle");
    Bytes::from(data)
}

// The early-reject checks of RFC 9171 Section 5.4: a streamed bundle that
// fails a header-only check, on lifetime or hop count, is dropped *before*
// its payload tail is drained, so the CLA never spools a large invalid
// payload. The count of segments left un-pulled in the `SegmentReceiver`
// asserts it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn segmented_gate_drops_before_draining_payload() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).build();
    bpa.start(false).await.unwrap();

    let (factory, service_rx) = TestService::factory();
    bpa.register_service(Some(ServiceId::Ipn(42)), factory)
        .await
        .unwrap();

    let (cla, _forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();
    let sink = || cla.sink.clone();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let local_dest: Eid = "ipn:0.1.42".parse().unwrap();
    let payload = vec![0xA5_u8; 20_000];

    // Expired, so the lifetime check drops it and the payload tail stays
    // un-pulled.
    let expired = build_expired_bundle(&remote_source, &local_dest, &payload);
    let mut stream = SegmentReceiver::new(&expired, 1000);
    assert_eq!(
        sink().dispatch(None, None, &mut stream).await.unwrap(),
        cla::Acceptance::Accepted
    );
    assert!(
        stream.remaining() > 0,
        "expired bundle must be dropped before the payload tail is drained"
    );

    // Hop-exhausted, so the hop count check drops it, with the same
    // expectation.
    let hopped = build_hop_exhausted_bundle(&remote_source, &local_dest, &payload);
    let mut stream = SegmentReceiver::new(&hopped, 1000);
    assert_eq!(
        sink().dispatch(None, None, &mut stream).await.unwrap(),
        cla::Acceptance::Accepted
    );
    assert!(
        stream.remaining() > 0,
        "hop-exhausted bundle must be dropped before the payload tail is drained"
    );

    // A valid bundle of the same size passes the checks, drains fully and
    // delivers, which shows the un-pulled segments above come from the
    // checks rather than from a stalled stream.
    let valid = build_bundle(&remote_source, &local_dest, &payload);
    let mut stream = SegmentReceiver::new(&valid, 1000);
    assert_eq!(
        sink().dispatch(None, None, &mut stream).await.unwrap(),
        cla::Acceptance::Accepted
    );
    assert_eq!(
        stream.remaining(),
        0,
        "a valid bundle drains its whole payload"
    );

    let (_src, delivered) =
        // Event-driven wait; the timeout only bounds a regression.
        tokio::time::timeout(tokio::time::Duration::from_secs(5), service_rx.recv_async())
            .await
            .expect("Timeout waiting for the valid control bundle")
            .expect("Channel closed");
    assert_eq!(delivered.as_ref(), payload.as_slice());
    assert!(
        service_rx.is_empty(),
        "neither gated bundle should have been delivered"
    );

    bpa.shutdown().await;
}

// The two checks report differently. A hop-exhausted arrival with the
// report flags set emits the combined report of RFC 9171 Sections 5.6 and
// 5.10, one Section 6.1.1 record asserting both reception and deletion and
// citing `HopLimitExceeded`, while an already-expired arrival with the same
// flags emits nothing: it is treated as never having arrived, which keeps
// an expired flood from being amplified. Swapping the two branches fails
// both halves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_reports_hop_exhaustion_but_not_expiry() {
    use hardy_bpv7::status_report::{AdministrativeRecord, ReasonCode};

    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .status_reports(true)
        .build();
    bpa.start(false).await.unwrap();

    // A CLA with a peer for the remote node, the route for the reports,
    // report-to defaulting to the source.
    let (cla, forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();
    let peer_addr = cla::Address::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let dest: Eid = "ipn:0.2.99".parse().unwrap();
    let report_flags = Flags {
        receipt_report_requested: true,
        delete_report_requested: true,
        ..Default::default()
    };

    // Hop-exhausted, report-requesting transit bundle.
    let (_, data) = Builder::new(remote_source.clone(), dest.clone())
        .with_flags(report_flags.clone())
        .with_hop_count(&HopInfo {
            limit: NonZeroU8::new(1).unwrap(),
            count: 2,
        })
        .with_payload(Cow::Borrowed(b"opaque".as_slice()))
        .build(CreationTimestamp::now())
        .unwrap();
    let mut inbound = Bytes::from(data);
    assert_eq!(
        cla.sink
            .dispatch(Some(&remote_node), None, &mut inbound)
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // One combined status report comes out of the CLA, asserting reception
    // and deletion in a single record, and the bundle itself is not
    // forwarded. Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for a status report")
    .expect("Channel closed");
    let parsed = parse(forwarded).expect("Failed to parse forwarded bundle");
    assert!(
        parsed.bundle.primary.flags.is_admin_record,
        "only status reports may leave the node for a gated bundle"
    );
    assert_eq!(parsed.bundle.primary.destination, remote_source);
    let body = parsed
        .bundle
        .blocks
        .get(&1)
        .expect("report has a payload block")
        .payload(&parsed.data)
        .expect("report payload in bundle");
    let AdministrativeRecord::BundleStatusReport(status) =
        hardy_cbor::decode::parse(body).expect("report payload is an admin record");
    assert_eq!(status.bundle_id.source, remote_source);
    assert!(status.received.is_some(), "reception asserted");
    assert!(status.deleted.is_some(), "deletion asserted");
    assert_eq!(status.reason, ReasonCode::HopLimitExceeded);

    // An already-expired arrival with the same report flags reports
    // nothing.
    let past = time::OffsetDateTime::now_utc() - time::Duration::hours(1);
    let timestamp = CreationTimestamp::from_parts(Some(DtnTime::saturating_from(past)), 1);
    let (_, data) = Builder::new(remote_source.clone(), dest)
        .with_flags(report_flags)
        .with_lifetime(Duration::from_secs(60))
        .with_payload(Cow::Borrowed(b"opaque".as_slice()))
        .build(timestamp)
        .unwrap();
    let mut inbound = Bytes::from(data);
    assert_eq!(
        cla.sink
            .dispatch(Some(&remote_node), None, &mut inbound)
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // The completed shutdown is the barrier proving the absence.
    bpa.shutdown().await;
    assert!(
        forwarded_rx.is_empty(),
        "an expired-at-arrival bundle must produce no reports at all"
    );
}

// A complete but invalid streamed payload, a CRC mismatch the drain's
// `ValidatingReceiver` detects after the header pass admitted the bundle, is
// accepted, terminated and reported as a header-check drop is: the reception
// and deletion pair of RFC 9171 Sections 5.6 and 5.10, the deletion citing
// `BlockUnintelligible`. The transfer is accepted rather than refused, the
// content not becoming valid by being resent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drain_failure_reports_reception_then_deletion() {
    use hardy_bpv7::status_report::{AdministrativeRecord, ReasonCode};

    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .status_reports(true)
        .build();
    bpa.start(false).await.unwrap();

    // A CLA with a peer for the remote node, the route for the reports,
    // report-to defaulting to the source.
    let (cla, forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();
    let peer_addr = cla::Address::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let dest: Eid = "ipn:0.2.99".parse().unwrap();

    // A bundle fed in CLA-sized chunks: its payload block is incomplete
    // when its header parses, so it takes the `Partial` route and the
    // payload streams through the validating drain. One corrupt byte deep in
    // the payload body fails the payload CRC there, after the header pass
    // admitted the bundle.
    let (_, data) = Builder::new(remote_source.clone(), dest)
        .with_flags(Flags {
            receipt_report_requested: true,
            delete_report_requested: true,
            ..Default::default()
        })
        .with_payload(Cow::Owned(vec![0xA5_u8; 50_000]))
        .build(CreationTimestamp::now())
        .unwrap();
    let mut data = data.into_vec();
    let corrupt_at = data.len() - 100; // Inside the payload body, before its CRC field.
    data[corrupt_at] ^= 0xFF;

    let mut stream = SegmentReceiver::new(&data, 1000);
    assert_eq!(
        cla.sink
            .dispatch(Some(&remote_node), None, &mut stream)
            .await
            .unwrap(),
        cla::Acceptance::Accepted,
        "a complete-but-corrupt transfer is accepted and terminated, never refused"
    );

    // One combined status report comes out of the CLA, asserting reception
    // and deletion in a single record, and the bundle itself is not
    // forwarded. Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for a status report")
    .expect("Channel closed");
    let parsed = parse(forwarded).expect("Failed to parse forwarded bundle");
    assert!(
        parsed.bundle.primary.flags.is_admin_record,
        "only status reports may leave the node for a drain-dropped bundle"
    );
    assert_eq!(parsed.bundle.primary.destination, remote_source);
    let body = parsed
        .bundle
        .blocks
        .get(&1)
        .expect("report has a payload block")
        .payload(&parsed.data)
        .expect("report payload in bundle");
    let AdministrativeRecord::BundleStatusReport(status) =
        hardy_cbor::decode::parse(body).expect("report payload is an admin record");
    assert_eq!(status.bundle_id.source, remote_source);
    assert!(status.received.is_some(), "reception asserted");
    assert!(status.deleted.is_some(), "deletion asserted");
    assert_eq!(status.reason, ReasonCode::BlockUnintelligible);

    // The completed shutdown is the barrier proving nothing else left the
    // node: one report bundle, and never the rejected bundle itself.
    bpa.shutdown().await;
    assert!(
        forwarded_rx.is_empty(),
        "exactly one report leaves the node"
    );
}

// A header-pass failure with a recoverable bundle id, here an unrecognised
// extension block flagged `delete_bundle_on_failure`, is reported as a
// header-check or drain drop is: one status report asserting both reception
// and deletion, citing `BlockUnsupported`, the delete-bundle case of RFC
// 9171 Section 5.6 Step 4. The transfer is accepted, the content not
// becoming valid by being resent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn header_failure_reports_reception_then_deletion() {
    use hardy_bpv7::status_report::{AdministrativeRecord, ReasonCode};

    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .status_reports(true)
        .build();
    bpa.start(false).await.unwrap();

    // A CLA with a peer for the remote node, the route for the reports,
    // report-to defaulting to the source.
    let (cla, forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();
    let peer_addr = cla::Address::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let dest: Eid = "ipn:0.2.99".parse().unwrap();

    // A report-requesting bundle carrying an unrecognised block, type 999
    // and block number 2, flagged `delete_bundle_on_failure`, spliced
    // between the primary block and the payload.
    let (_, data) = Builder::new(remote_source.clone(), dest)
        .with_flags(Flags {
            receipt_report_requested: true,
            delete_report_requested: true,
            ..Default::default()
        })
        .with_payload(Cow::Borrowed(b"opaque"))
        .build(CreationTimestamp::now())
        .unwrap();
    let unknown = emit_array(Some(5), |a| {
        a.emit(&999u64); // unrecognised block type
        a.emit(&2u64); // block number
        a.emit(&0x04u64); // flags: delete_bundle_on_failure
        a.emit(&0u64); // CRC type: none
        a.emit(&hardy_cbor::encode::Bytes(&[0xDE, 0xAD]));
    });
    assert_eq!(data[0], 0x9F, "bundle is an indefinite array");
    let (_, primary_len) = skip_value(&data[1..], 16).expect("skip primary");
    let insert = 1 + primary_len;
    let mut modified = Vec::with_capacity(data.len() + unknown.len());
    modified.extend_from_slice(&data[..insert]);
    modified.extend_from_slice(&unknown);
    modified.extend_from_slice(&data[insert..]);
    let mut inbound = Bytes::from(modified);

    assert_eq!(
        cla.sink
            .dispatch(Some(&remote_node), None, &mut inbound)
            .await
            .unwrap(),
        cla::Acceptance::Accepted,
        "a complete-but-unsupported bundle is accepted and terminated, never refused"
    );

    // One combined status report comes out of the CLA, asserting reception
    // and deletion in a single record, and the bundle itself is not
    // forwarded. Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for a status report")
    .expect("Channel closed");
    let parsed = parse(forwarded).expect("Failed to parse forwarded bundle");
    assert!(
        parsed.bundle.primary.flags.is_admin_record,
        "only status reports may leave the node for a rejected bundle"
    );
    assert_eq!(parsed.bundle.primary.destination, remote_source);
    let body = parsed
        .bundle
        .blocks
        .get(&1)
        .expect("report has a payload block")
        .payload(&parsed.data)
        .expect("report payload in bundle");
    let AdministrativeRecord::BundleStatusReport(status) =
        hardy_cbor::decode::parse(body).expect("report payload is an admin record");
    assert_eq!(status.bundle_id.source, remote_source);
    assert!(status.received.is_some(), "reception asserted");
    assert!(status.deleted.is_some(), "deletion asserted");
    assert_eq!(status.reason, ReasonCode::BlockUnsupported);

    // The completed shutdown is the barrier proving nothing else left the
    // node: one report bundle, and never the rejected bundle itself.
    bpa.shutdown().await;
    assert!(
        forwarded_rx.is_empty(),
        "exactly one report leaves the node"
    );
}

// RFC 9171 Section 5.6 Step 4 reports before it deletes: an unrecognised
// block flagged both `report_on_failure` and `delete_bundle_on_failure`, on
// a bundle requesting no status reports at bundle level, still raises its
// reception report citing `BlockUnsupported`. The report asserts no
// deletion, none having been requested, and the deletion the second flag
// orders shows as the bundle never leaving the node.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn step4_block_flag_report_survives_delete_bundle() {
    use hardy_bpv7::status_report::{AdministrativeRecord, ReasonCode};

    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .status_reports(true)
        .build();
    bpa.start(false).await.unwrap();

    let (cla, forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();
    let peer_addr = cla::Address::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let dest: Eid = "ipn:0.2.99".parse().unwrap();

    // No bundle-level report flags, and an unrecognised block, type 999 and
    // block number 2, flagged `report_on_failure` and
    // `delete_bundle_on_failure`, spliced after the primary block.
    let base = build_bundle(&remote_source, &dest, b"payload");
    let unknown = emit_array(Some(5), |a| {
        a.emit(&999u64); // unrecognised block type
        a.emit(&2u64); // block number
        a.emit(&0x06u64); // flags: report_on_failure | delete_bundle_on_failure
        a.emit(&0u64); // CRC type: none
        a.emit(&hardy_cbor::encode::Bytes(&[0xDE, 0xAD]));
    });
    assert_eq!(base[0], 0x9F, "bundle is an indefinite array");
    let (_, primary_len) = skip_value(&base[1..], 16).expect("skip primary");
    let insert = 1 + primary_len;
    let mut modified = Vec::with_capacity(base.len() + unknown.len());
    modified.extend_from_slice(&base[..insert]);
    modified.extend_from_slice(&unknown);
    modified.extend_from_slice(&base[insert..]);
    let mut inbound = Bytes::from(modified);

    assert_eq!(
        cla.sink
            .dispatch(Some(&remote_node), None, &mut inbound)
            .await
            .unwrap(),
        cla::Acceptance::Accepted,
        "a complete-but-unsupported bundle is accepted and terminated, never refused"
    );

    // Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for the block-flag reception report")
    .expect("Channel closed");
    let parsed = parse(forwarded).expect("Failed to parse forwarded bundle");
    assert!(
        parsed.bundle.primary.flags.is_admin_record,
        "only the status report may leave the node"
    );
    let body = parsed
        .bundle
        .blocks
        .get(&1)
        .expect("report has a payload block")
        .payload(&parsed.data)
        .expect("report payload in bundle");
    let AdministrativeRecord::BundleStatusReport(status) =
        hardy_cbor::decode::parse(body).expect("report payload is an admin record");
    assert_eq!(status.bundle_id.source, remote_source);
    assert!(status.received.is_some(), "reception asserted");
    assert!(status.deleted.is_none(), "no deletion report was requested");
    assert_eq!(status.reason, ReasonCode::BlockUnsupported);

    // The completed shutdown is the barrier proving nothing else left.
    bpa.shutdown().await;
    assert!(
        forwarded_rx.is_empty(),
        "exactly one report leaves the node"
    );
}

// A rejected bundle that requested reception reporting alone still carries
// the facts of RFC 9171 Section 5.6 Step 4 on its reception-only status
// report: an unrecognised `report_on_failure` block sets the reception
// reason to `BlockUnsupported`, and a hop-exhausted drop does not reduce it
// to `NoAdditionalInformation`. With no deletion asserted, the record's one
// reason belongs to the reception assertion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reception_only_reject_carries_step4_reason() {
    use hardy_bpv7::status_report::{AdministrativeRecord, ReasonCode};

    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .status_reports(true)
        .build();
    bpa.start(false).await.unwrap();

    let (cla, forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();
    let peer_addr = cla::Address::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let dest: Eid = "ipn:0.2.99".parse().unwrap();

    // A hop-exhausted, reception-report-only bundle carrying an
    // unrecognised block, type 999 and block number 3, the builder's Hop
    // Count block taking 2, flagged `report_on_failure`.
    let (_, data) = Builder::new(remote_source.clone(), dest)
        .with_flags(Flags {
            receipt_report_requested: true,
            ..Default::default()
        })
        .with_hop_count(&HopInfo {
            limit: NonZeroU8::new(1).unwrap(),
            count: 2,
        })
        .with_payload(Cow::Borrowed(b"opaque"))
        .build(CreationTimestamp::now())
        .unwrap();
    let unknown = emit_array(Some(5), |a| {
        a.emit(&999u64); // unrecognised block type
        a.emit(&3u64); // block number
        a.emit(&0x02u64); // flags: report_on_failure
        a.emit(&0u64); // CRC type: none
        a.emit(&hardy_cbor::encode::Bytes(&[0xDE, 0xAD]));
    });
    assert_eq!(data[0], 0x9F, "bundle is an indefinite array");
    let (_, primary_len) = skip_value(&data[1..], 16).expect("skip primary");
    let insert = 1 + primary_len;
    let mut modified = Vec::with_capacity(data.len() + unknown.len());
    modified.extend_from_slice(&data[..insert]);
    modified.extend_from_slice(&unknown);
    modified.extend_from_slice(&data[insert..]);
    let mut inbound = Bytes::from(modified);

    assert_eq!(
        cla.sink
            .dispatch(Some(&remote_node), None, &mut inbound)
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // One reception-only report, asserting reception with the Step 4 reason
    // and no deletion, deletion reports not having been requested.
    // Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for a status report")
    .expect("Channel closed");
    let parsed = parse(forwarded).expect("Failed to parse forwarded bundle");
    assert!(parsed.bundle.primary.flags.is_admin_record);
    assert_eq!(parsed.bundle.primary.destination, remote_source);
    let body = parsed
        .bundle
        .blocks
        .get(&1)
        .expect("report has a payload block")
        .payload(&parsed.data)
        .expect("report payload in bundle");
    let AdministrativeRecord::BundleStatusReport(status) =
        hardy_cbor::decode::parse(body).expect("report payload is an admin record");
    assert!(status.received.is_some(), "reception asserted");
    assert!(
        status.deleted.is_none(),
        "no deletion asserted without the request flag"
    );
    assert_eq!(status.reason, ReasonCode::BlockUnsupported);

    // The completed shutdown is the barrier proving nothing else left the
    // node: one report bundle, and never the rejected bundle itself.
    bpa.shutdown().await;
    assert!(
        forwarded_rx.is_empty(),
        "exactly one report leaves the node"
    );
}

// A block flag alone triggers RFC 9171 Section 5.6 Step 4: a bundle
// requesting no status reports at bundle level, carrying an unrecognised
// block flagged `report_on_failure`, still generates a reception report
// citing `BlockUnsupported`, the block's flag being the request. The bundle
// itself is forwarded intact, unrecognised block and all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn step4_block_flag_alone_forces_reception_report() {
    use hardy_bpv7::status_report::{AdministrativeRecord, ReasonCode};

    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .status_reports(true)
        .build();
    bpa.start(false).await.unwrap();

    let (cla, forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();
    let peer_addr = cla::Address::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let dest: Eid = "ipn:0.2.99".parse().unwrap();

    // No bundle-level report flags, and an unrecognised block, type 999 and
    // block number 2, flagged `report_on_failure`, spliced after the primary
    // block.
    let base = build_bundle(&remote_source, &dest, b"payload");
    let unknown = emit_array(Some(5), |a| {
        a.emit(&999u64); // unrecognised block type
        a.emit(&2u64); // block number
        a.emit(&0x02u64); // flags: report_on_failure
        a.emit(&0u64); // CRC type: none
        a.emit(&hardy_cbor::encode::Bytes(&[0xDE, 0xAD]));
    });
    assert_eq!(base[0], 0x9F, "bundle is an indefinite array");
    let (_, primary_len) = skip_value(&base[1..], 16).expect("skip primary");
    let insert = 1 + primary_len;
    let mut modified = Vec::with_capacity(base.len() + unknown.len());
    modified.extend_from_slice(&base[..insert]);
    modified.extend_from_slice(&unknown);
    modified.extend_from_slice(&base[insert..]);
    let mut inbound = Bytes::from(modified);

    assert_eq!(
        cla.sink
            .dispatch(Some(&remote_node), None, &mut inbound)
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // Two bundles leave the node in either order: the reception report, to
    // the source, and the forwarded original.
    let mut report = None;
    let mut original = None;
    for _ in 0..2 {
        // Event-driven wait; the timeout only bounds a regression.
        let forwarded = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            forwarded_rx.recv_async(),
        )
        .await
        .expect("Timeout waiting for a forwarded bundle")
        .expect("Channel closed");
        let parsed = parse(forwarded).expect("Failed to parse forwarded bundle");
        if parsed.bundle.primary.flags.is_admin_record {
            report = Some(parsed);
        } else {
            original = Some(parsed);
        }
    }

    // The original forwards intact, a report flag never removing the
    // block.
    let original = original.expect("original bundle forwarded");
    assert!(
        original
            .bundle
            .blocks
            .values()
            .any(|b| b.block_type == Type::Unrecognised(999)),
        "the unrecognised block rides on unchanged"
    );

    // The report asserts reception alone, citing the Step 4 reason.
    let report = report.expect("block-demanded reception report emitted");
    assert_eq!(report.bundle.primary.destination, remote_source);
    let body = report
        .bundle
        .blocks
        .get(&1)
        .expect("report has a payload block")
        .payload(&report.data)
        .expect("report payload in bundle");
    let AdministrativeRecord::BundleStatusReport(status) =
        hardy_cbor::decode::parse(body).expect("report payload is an admin record");
    assert!(status.received.is_some(), "reception asserted");
    assert!(status.deleted.is_none(), "nothing was deleted");
    assert_eq!(status.reason, ReasonCode::BlockUnsupported);

    bpa.shutdown().await;
}

// The same block-demanded bundle, with `dtn:none` as its report-to,
// produces no report, there being nowhere to send it. A bundle with no
// bundle-level report flags may carry a null report-to, only the flag
// combinations of RFC 9171 Section 4.2.3 being parse-rejected. The bundle
// still forwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn step4_forced_report_suppressed_for_null_report_to() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .status_reports(true)
        .build();
    bpa.start(false).await.unwrap();

    let (cla, forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();
    let peer_addr = cla::Address::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let dest: Eid = "ipn:0.2.99".parse().unwrap();

    let (_, data) = Builder::new(remote_source.clone(), dest.clone())
        .with_report_to("dtn:none".parse().unwrap())
        .with_payload(Cow::Borrowed(b"payload"))
        .build(CreationTimestamp::now())
        .unwrap();
    let unknown = emit_array(Some(5), |a| {
        a.emit(&999u64); // unrecognised block type
        a.emit(&2u64); // block number
        a.emit(&0x02u64); // flags: report_on_failure
        a.emit(&0u64); // CRC type: none
        a.emit(&hardy_cbor::encode::Bytes(&[0xDE, 0xAD]));
    });
    assert_eq!(data[0], 0x9F, "bundle is an indefinite array");
    let (_, primary_len) = skip_value(&data[1..], 16).expect("skip primary");
    let insert = 1 + primary_len;
    let mut modified = Vec::with_capacity(data.len() + unknown.len());
    modified.extend_from_slice(&data[..insert]);
    modified.extend_from_slice(&unknown);
    modified.extend_from_slice(&data[insert..]);
    let mut inbound = Bytes::from(modified);

    assert_eq!(
        cla.sink
            .dispatch(Some(&remote_node), None, &mut inbound)
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // Only the original leaves the node.
    // Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for the forwarded bundle")
    .expect("Channel closed");
    let parsed = parse(forwarded).expect("Failed to parse forwarded bundle");
    assert!(
        !parsed.bundle.primary.flags.is_admin_record,
        "no report may be addressed to the null endpoint"
    );
    assert_eq!(parsed.bundle.primary.destination, dest);

    // The completed shutdown is the barrier proving the absence.
    bpa.shutdown().await;
    assert!(
        forwarded_rx.is_empty(),
        "exactly one bundle leaves the node"
    );
}

// A transfer that delivers its `Final` with the bundle short is complete.
// The bundle is invalid, so it is accepted and dropped rather than refused,
// a custodial peer otherwise resending the same bytes until they expire,
// and its headers parsed, so the drop is reported. A single `Final`
// carrying a bundle whose payload is cut short is never handed to the
// payload drain, whose stream is exhausted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_short_bundle_in_one_final_is_reported() {
    assert_short_final_is_reported(short_bundle(), None).await;
}

// The same short bundle in 1000-byte segments: the header pass reaches
// `Partial` on a `Next`, so the short payload ends in the payload drain's
// `Final`, which reports it the same way.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_short_bundle_in_segments_is_reported() {
    assert_short_final_is_reported(short_bundle(), Some(1000)).await;
}

// A bundle short by less than one parser chunk, in a single `Final`, is
// reported too, the parser not awaiting bytes that never come.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_final_short_within_one_parser_chunk_is_reported() {
    let full = reporting_bundle(10_000);
    assert_short_final_is_reported(full.slice(..full.len() - 100), None).await;
}

// The same short bytes without their `Final` are an abandoned transfer
// rather than a reception: refused, so that the peer may resend, and never
// reported.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_abandoned_short_transfer_is_refused_and_unreported() {
    let (bpa, _app, service_rx, cla, forwarded_rx) = short_final_setup().await;
    let segments = short_bundle()
        .chunks(1000)
        .map(|chunk| Segment::Next(Bytes::copy_from_slice(chunk)))
        .collect();
    assert_eq!(
        dispatch_abandoned(&cla, segments).await,
        cla::Acceptance::Refused
    );

    // The completed shutdown is the barrier proving the absence.
    bpa.shutdown().await;
    assert!(
        service_rx.is_empty(),
        "an abandoned transfer must not deliver"
    );
    assert!(
        forwarded_rx.is_empty(),
        "an abandoned transfer is never reported"
    );
}

// A bundle from the remote source to the local service, requesting
// reception and deletion reports.
fn reporting_bundle(payload_len: usize) -> Bytes {
    let (_, data) = Builder::new("ipn:0.2.1".parse().unwrap(), "ipn:0.1.42".parse().unwrap())
        .with_flags(Flags {
            receipt_report_requested: true,
            delete_report_requested: true,
            ..Default::default()
        })
        .with_payload(Cow::Owned(vec![0xA5_u8; payload_len]))
        .build(CreationTimestamp::now())
        .unwrap();
    Bytes::from(data)
}

// A reporting bundle with a 20 KB payload cut to 8 KB: the payload's
// byte-string header still declares 20 KB, but the body is short.
fn short_bundle() -> Bytes {
    reporting_bundle(20_000).slice(..8_000)
}

// A node that observes how a short transfer ends: status reports on, the
// local service registered, and a peer for the remote source's node, the
// route for the reports, report-to defaulting to the source.
async fn short_final_setup() -> (
    Bpa,
    Arc<TestService>,
    flume::Receiver<(Eid, Bytes)>,
    Arc<PipelineCla>,
    flume::Receiver<Bytes>,
) {
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
        .status_reports(true)
        .build();
    bpa.start(false).await.unwrap();
    let (factory, service_rx) = TestService::factory();
    let service = bpa
        .register_service(Some(ServiceId::Ipn(42)), factory)
        .await
        .unwrap();
    let (cla, forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .add_peer(
            cla::Address::Private("peer".as_bytes().into()),
            from_ref(&remote_node),
        )
        .await
        .unwrap();
    (bpa, service, service_rx, cla, forwarded_rx)
}

// Dispatches `short` in `chunk`-byte segments ending in a `Final`, or in a
// single `Final` for `None`, and asserts it is rejected with one report.
async fn assert_short_final_is_reported(short: Bytes, chunk: Option<usize>) {
    let mut stream = SegmentReceiver::new(&short, chunk.unwrap_or(short.len()));
    assert_rejected_with_one_report(&mut stream).await;
}

// Dispatches `stream`, a complete transfer of an invalid bundle, and
// asserts that the transfer is accepted, the bundle never delivered, and
// exactly one status report leaves, asserting reception and deletion, the
// deletion citing `BlockUnintelligible`.
async fn assert_rejected_with_one_report(stream: &mut SegmentReceiver) {
    use hardy_bpv7::status_report::{AdministrativeRecord, ReasonCode};

    let (bpa, _app, service_rx, cla, forwarded_rx) = short_final_setup().await;

    // The timeout only bounds a regression: a `Final` handed to the payload
    // drain after the header pass took it would await an exhausted stream.
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        cla.sink.dispatch(None, None, stream),
    )
    .await
    .expect("a complete transfer must not hang the ingress task");
    assert!(
        matches!(result, Ok(cla::Acceptance::Accepted)),
        "an invalid bundle in a complete transfer is accepted and dropped: {result:?}"
    );

    // Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(Duration::from_secs(5), forwarded_rx.recv_async())
        .await
        .expect("Timeout waiting for the status report")
        .expect("Channel closed");
    let parsed = parse(forwarded).expect("Failed to parse forwarded bundle");
    assert!(
        parsed.bundle.primary.flags.is_admin_record,
        "only the status report may leave the node"
    );
    let body = parsed
        .bundle
        .blocks
        .get(&1)
        .expect("report has a payload block")
        .payload(&parsed.data)
        .expect("report payload in bundle");
    let AdministrativeRecord::BundleStatusReport(status) =
        hardy_cbor::decode::parse(body).expect("report payload is an admin record");
    assert_eq!(status.bundle_id.source, "ipn:0.2.1".parse::<Eid>().unwrap());
    assert!(status.received.is_some(), "reception asserted");
    assert!(status.deleted.is_some(), "deletion asserted");
    assert_eq!(status.reason, ReasonCode::BlockUnintelligible);

    // The completed shutdown is the barrier proving nothing else left the
    // node, and that the bundle was never delivered.
    bpa.shutdown().await;
    assert!(
        forwarded_rx.is_empty(),
        "exactly one report leaves the node"
    );
    assert!(
        service_rx.is_empty(),
        "the invalid bundle must not be delivered"
    );
}

// A payload CRC mismatch is reported however the bundle arrives, whole in
// one `Final` or whole in a `Next` that an empty `Final` closes. The payload
// tail checks the trailer in the buffer as it would in a later segment.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_payload_crc_mismatch_is_reported_however_it_arrives() {
    let mut bad = reporting_bundle(1000).to_vec();
    // The last byte of the CRC-32 value, the trailer being
    // `44 c0 c1 c2 c3 FF`.
    let last_crc_byte = bad.len() - 2;
    bad[last_crc_byte] ^= 0xFF;
    let bad = Bytes::from(bad);

    assert_short_final_is_reported(bad.clone(), None).await;

    let mut stream = SegmentReceiver {
        segments: Mutex::new(VecDeque::from([
            cla::Segment::Next(bad),
            cla::Segment::Final(Bytes::new()),
        ])),
    };
    assert_rejected_with_one_report(&mut stream).await;
}

// R-04: the ingress size cap refuses an over-cap bundle at both of
// `parse_headers`' enforcement points, the accumulated header bytes and the
// declared whole-bundle size, each a `HeaderFailure::TooLarge`, surfacing
// `Acceptance::Refused` so that the CLA withholds the transfer ack. The
// declared case refuses without awaiting the payload.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ingress_size_cap_refuses_oversized_bundle() {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let local_dest: Eid = "ipn:0.1.42".parse().unwrap();
    let inbound = build_bundle(&remote_source, &local_dest, &vec![0xA5_u8; 20_000]);

    // A cap below the first accumulated segment trips `parse_headers`
    // before the header chain completes.
    {
        let bpa = Bpa::builder()
            .node_ids(node_ids.clone())
            .max_bundle_size(NonZeroU64::new(256).unwrap())
            .build();
        bpa.start(false).await.unwrap();
        let (cla, _fwd) = PipelineCla::factory();
        let cla = bpa.register_cla("test", cla).await.unwrap();
        let mut stream = SegmentReceiver::new(&inbound, 1000);
        assert_eq!(
            cla.sink.dispatch(None, None, &mut stream).await.unwrap(),
            cla::Acceptance::Refused,
            "header-phase over-cap must be refused"
        );
        bpa.shutdown().await;
    }

    // A cap that admits the header chain, parsing to `Partial`, but not the
    // declared payload refuses in the header pass, before a payload byte is
    // drained, which the source parking forever after the header chunks
    // shows. The timeout only bounds a regression.
    {
        let bpa = Bpa::builder()
            .node_ids(node_ids.clone())
            .max_bundle_size(NonZeroU64::new(10_000).unwrap())
            .build();
        bpa.start(false).await.unwrap();
        let (cla, _fwd) = PipelineCla::factory();
        let cla = bpa.register_cla("test", cla).await.unwrap();
        let mut stream = ParkedReceiver::new(&inbound, 1000, 3);
        assert_eq!(
            tokio::time::timeout(
                tokio::time::Duration::from_secs(5),
                cla.sink.dispatch(None, None, &mut stream)
            )
            .await
            .expect("Timeout: the declared over-cap refusal must not wait for the payload")
            .unwrap(),
            cla::Acceptance::Refused,
            "declared over-cap must be refused pre-drain"
        );
        bpa.shutdown().await;
    }
}

// A payload declaration whose wire form ends at `u64::MAX` leaves no room
// for the outer break. The header pass rejects it as invalid, so CLA ingress
// disposes of it, accepted and never stored, without pulling a payload byte
// and without computing the bundle's length, which would overflow.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ingress_disposes_of_a_payload_declared_to_u64_max() {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).build();
    bpa.start(false).await.unwrap();
    let (cla, _fwd) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();

    // A built bundle with its payload block replaced by a CRC-32 payload
    // header (`86 01 01 04 02 5b` + an 8-byte length) whose extent ends at
    // exactly u64::MAX:
    //   extent.end = block_start + header(14) + len + crc_trailer(5)
    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let local_dest: Eid = "ipn:0.1.42".parse().unwrap();
    let good = build_bundle(&remote_source, &local_dest, b"x");
    let block_start = parse(good.clone())
        .expect("built bundle parses")
        .bundle
        .blocks
        .get(&1)
        .expect("payload block")
        .extent
        .start;
    let len = u64::MAX - block_start - 19;
    let mut crafted = good[..usize::try_from(block_start).unwrap()].to_vec();
    crafted.extend_from_slice(&[0x86, 0x01, 0x01, 0x04, 0x02, 0x5b]);
    crafted.extend_from_slice(&len.to_be_bytes());

    // The headers arrive whole and the source then parks forever, so a
    // header pass that admitted the declaration would await the payload. The
    // timeout only bounds a regression.
    let mut stream = ParkedReceiver::new(&crafted, crafted.len(), 1);
    assert_eq!(
        tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            cla.sink.dispatch(None, None, &mut stream)
        )
        .await
        .expect("Timeout: the rejection must not wait for the payload")
        .unwrap(),
        cla::Acceptance::Accepted,
    );

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// PERF-01: Throughput (REQ-13: >1000 bundles/sec)
// ---------------------------------------------------------------------------

/// Measures forwarding throughput through the BPA pipeline, dispatching
/// bundles through a CLA, routing them to a peer and signalling on the
/// other side.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn throughput() {
    print_system_info();
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();

    let bpa = Bpa::builder().node_ids(node_ids).build();
    bpa.start(false).await.unwrap();

    let (cla, arrival_rx) = TimedCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();

    let peer_addr = cla::Address::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    let src: Eid = "ipn:0.3.1".parse().unwrap();
    let dst: Eid = "ipn:0.2.99".parse().unwrap();
    let count = 1000usize;

    // The bundles, each with a unique id, are built up front, to keep
    // construction out of the measurement.
    let warmup_bundles: Vec<_> = (0..10)
        .map(|_| build_bundle(&src, &dst, b"warmup"))
        .collect();
    let test_bundles: Vec<_> = (0..count)
        .map(|_| build_bundle(&src, &dst, b"throughput"))
        .collect();

    // Warm up the pipeline.
    for (i, mut bundle) in warmup_bundles.into_iter().enumerate() {
        assert_eq!(
            cla.sink.dispatch(None, None, &mut bundle).await.unwrap(),
            cla::Acceptance::Accepted
        );
        // Event-driven wait; the timeout only bounds a regression.
        tokio::time::timeout(tokio::time::Duration::from_secs(5), arrival_rx.recv_async())
            .await
            .unwrap_or_else(|_| panic!("Timeout waiting for warmup bundle {i}"))
            .unwrap();
    }

    // Dispatch and receive each bundle before sending the next, which keeps
    // backpressure and duplicates out of the measurement.
    let start = tokio::time::Instant::now();
    let mut last_arrival = start;
    for (i, mut bundle) in test_bundles.into_iter().enumerate() {
        assert_eq!(
            cla.sink.dispatch(None, None, &mut bundle).await.unwrap(),
            cla::Acceptance::Accepted
        );
        last_arrival =
            // Event-driven wait; the timeout only bounds a regression.
            tokio::time::timeout(tokio::time::Duration::from_secs(5), arrival_rx.recv_async())
                .await
                .unwrap_or_else(|_| {
                    panic!("Timeout waiting for throughput bundle {i} (of {count})")
                })
                .unwrap();
    }
    let elapsed = last_arrival - start;

    let bundles_per_sec = count as f64 / elapsed.as_secs_f64();
    eprintln!("Throughput: {count} bundles in {elapsed:.2?} = {bundles_per_sec:.0} bundles/sec",);

    // The criterion benchmark measures REQ-13, more than 1000 bundles per
    // second in memory, rather than this test gating it: a wall-clock gate
    // here fails under build load. This test pins that every bundle arrives,
    // and reports the rate.

    // `bpa.shutdown()` is not called: 1000 `ForwardPending` bundles in
    // metadata leave the poller re-polling indefinitely through shutdown.
    // The BPA is leaked, and the runtime cleans up as the test exits.
}

// ---------------------------------------------------------------------------
// PERF-LAT-01: Forwarding Latency
// ---------------------------------------------------------------------------

/// Measures per-bundle forwarding latency through the BPA pipeline, one
/// way: a CLA dispatch, the BPA's routing, then a CLA forward.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forwarding_latency() {
    print_system_info();
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();

    let bpa = Bpa::builder().node_ids(node_ids).build();
    bpa.start(false).await.unwrap();

    let (cla, arrival_rx) = TimedCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();

    let peer_addr = cla::Address::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    let src: Eid = "ipn:0.3.1".parse().unwrap();
    let dst: Eid = "ipn:0.2.99".parse().unwrap();
    let count = 100usize;

    // The bundles, each with a unique id, are built up front.
    let warmup_bundles: Vec<_> = (0..10)
        .map(|_| build_bundle(&src, &dst, b"warmup"))
        .collect();
    let test_bundles: Vec<_> = (0..count)
        .map(|_| build_bundle(&src, &dst, b"latency"))
        .collect();

    // Warm up
    for (i, mut bundle) in warmup_bundles.into_iter().enumerate() {
        assert_eq!(
            cla.sink.dispatch(None, None, &mut bundle).await.unwrap(),
            cla::Acceptance::Accepted
        );
        // Event-driven wait; the timeout only bounds a regression.
        tokio::time::timeout(tokio::time::Duration::from_secs(5), arrival_rx.recv_async())
            .await
            .unwrap_or_else(|_| panic!("Timeout waiting for warmup bundle {i}"))
            .unwrap();
    }

    // The arrival time is sampled inside `forward`, so the measurement
    // covers the pipeline's processing rather than the channel wait.
    let mut latencies = Vec::with_capacity(count);

    for (i, mut bundle) in test_bundles.into_iter().enumerate() {
        let dispatched = tokio::time::Instant::now();
        assert_eq!(
            cla.sink.dispatch(None, None, &mut bundle).await.unwrap(),
            cla::Acceptance::Accepted
        );
        let arrived =
            // Event-driven wait; the timeout only bounds a regression.
            tokio::time::timeout(tokio::time::Duration::from_secs(5), arrival_rx.recv_async())
                .await
                .unwrap_or_else(|_| panic!("Timeout waiting for bundle {i} (of {count})"))
                .unwrap();
        latencies.push(arrived - dispatched);
    }

    latencies.sort();
    let p50 = latencies[latencies.len() / 2];
    let p95 = latencies[latencies.len() * 95 / 100];
    let p99 = latencies[latencies.len() * 99 / 100];

    eprintln!("Forwarding latency ({count} bundles): P50={p50:.2?} P95={p95:.2?} P99={p99:.2?}");

    // Dropping the receiver unblocks any parked poller send.
    drop(arrival_rx);
    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// INT-BPA-06: Egress filter bundle/data consistency
// ---------------------------------------------------------------------------

// The block type the extent-checking Rewriter inserts.
const INSERTED: Type = Type::Unrecognised(200);

/// Records any divergence between the `Bundle`'s block extents and the wire
/// data it is handed alongside, seen through the reader's extent-based
/// block access, where a stale block map over the bytes reads back shifted
/// bytes. It then inserts a block, shifting the payload extent the per-hop
/// rewrite works over next.
struct ExtentCheckRewriter {
    mismatch: Arc<Mutex<Option<String>>>,
}

impl Rewriter for ExtentCheckRewriter {
    fn rewrite(&self, ctx: &mut RewriteContext<'_>) {
        let reader = ctx.reader();
        let mut mismatch = self.mismatch.lock().unwrap();

        // The payload extent indexes the bytes.
        match reader.block(1) {
            Some((_, Availability::Available(payload))) if payload.as_ref() == b"Hello remote" => {}
            Some((_, Availability::Available(payload))) => {
                *mismatch = Some(format!("payload extent skewed: {:?}", payload.as_ref()))
            }
            Some((_, unavailable)) => {
                *mismatch = Some(format!("payload unavailable: {unavailable:?}"))
            }
            None => *mismatch = Some("no payload block".to_string()),
        }

        if let Err(e) = ctx.editor().insert(
            INSERTED,
            BlockFlags::default(),
            CrcType::None,
            emit(&42u64).0.into(),
        ) {
            *mismatch = Some(format!("the insert was refused: {e}"));
        }
    }
}

/// The Egress Rewriters see a consistent `(bundle, data)` pair, and hand
/// the per-hop rewrite a consistent rewritten one: the transmitted bundle
/// carries the Rewriter's insert, this node's Previous Node block and the
/// payload, each decoding from its own extent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn egress_filter_sees_consistent_extents() {
    let mismatch = Arc::new(Mutex::new(None));
    let mut pack = FilterPack::new("test");
    pack.egress_rewriter(
        "extent-check",
        ExtentCheckRewriter {
            mismatch: mismatch.clone(),
        },
    );
    let bpa = Bpa::builder().add_filters(pack).build();
    bpa.start(false).await.unwrap();

    let (cla, forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();

    let peer_addr = cla::Address::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink.add_peer(peer_addr, &[remote_node]).await.unwrap();

    // A locally-originated bundle has no Previous Node block, so the
    // per-hop rewrite inserts one after the Rewriter's insert.
    let (factory, _service_rx) = TestService::factory();
    let service = bpa
        .register_service(Some(ServiceId::Ipn(42)), factory)
        .await
        .unwrap();
    let source = service.endpoint.clone();
    service
        .sink
        .send(
            "ipn:0.2.99".parse().unwrap(),
            Bytes::from_static(b"Hello remote"),
            Duration::from_secs(3600),
            None,
        )
        .await
        .unwrap();

    // Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for forwarded bundle")
    .expect("Channel closed");

    assert_eq!(
        *mismatch.lock().unwrap(),
        None,
        "Egress filter saw an inconsistent (bundle, data) pair"
    );

    let out = parse(forwarded).expect("the transmitted bundle parses");
    let block = |block_type: Type| {
        out.bundle
            .blocks
            .values()
            .find(|b| b.block_type == block_type)
            .expect("the block is transmitted")
    };
    assert_eq!(
        block(INSERTED).payload(&out.data),
        Some(emit(&42u64).0.as_slice()),
        "the Rewriter's insert"
    );
    let Eid::Ipn { fqnn, .. } = source else {
        panic!("the node has an ipn identity");
    };
    assert_eq!(
        block(Type::PreviousNode)
            .extract::<Eid>(&out.data)
            .expect("the Previous Node decodes")
            .expect("the Previous Node is resident"),
        Eid::Ipn {
            fqnn,
            service_number: 0
        },
        "this node's admin endpoint"
    );
    assert_eq!(
        out.bundle.blocks[&1].payload(&out.data),
        Some(b"Hello remote".as_slice())
    );

    bpa.shutdown().await;
}

/// Keys traffic for the unrouted ipn:0.3 node onto ipn:0.2.1, a per-bundle
/// override in the shape of a source route, keying only a foreign
/// destination.
struct KeyingClassifier;

impl Classifier for KeyingClassifier {
    fn classify(&self, ctx: &ClassifyContext<'_>) -> Verdict<MetadataDelta> {
        let mut delta = MetadataDelta::default();
        if let Eid::Ipn { fqnn, .. } = &ctx.bundle().primary.destination
            && fqnn.node_number == 3
        {
            delta.route_key = Some("ipn:0.2.1".parse().unwrap());
        }
        Verdict::Continue(delta)
    }
}

/// A Classifier-written `route_key` persists in the record's metadata and
/// drives the RIB lookup, so a bundle whose destination has no route
/// forwards over the route for its key. Without the key, the bundle parks
/// `Waiting` forever, which makes the forward the proof.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn classifier_route_key_drives_the_lookup() {
    let mut pack = FilterPack::new("test");
    pack.originate_classifier("keyer", KeyingClassifier);
    let bpa = Bpa::builder().add_filters(pack).build();
    bpa.start(false).await.unwrap();

    // A peer exists for ipn:0.2 alone, and ipn:0.3 has no route.
    let (cla, forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .add_peer(
            cla::Address::Private("peer".as_bytes().into()),
            &[remote_node],
        )
        .await
        .unwrap();

    let (factory, _service_rx) = TestService::factory();
    let service = bpa
        .register_service(Some(ServiceId::Ipn(42)), factory)
        .await
        .unwrap();
    service
        .sink
        .send(
            "ipn:0.3.99".parse().unwrap(),
            Bytes::from_static(b"Keyed onward"),
            Duration::from_secs(3600),
            None,
        )
        .await
        .unwrap();

    // Event-driven wait; the timeout only bounds a regression.
    tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for the keyed bundle to forward")
    .expect("Channel closed");

    bpa.shutdown().await;
}

/// Keys traffic for the unregistered local service ipn:0.1.77 onto
/// ipn:0.2.1, a foreign key for a local destination.
struct LocalKeyingClassifier;

impl Classifier for LocalKeyingClassifier {
    fn classify(&self, ctx: &ClassifyContext<'_>) -> Verdict<MetadataDelta> {
        let mut delta = MetadataDelta::default();
        if ctx.bundle().primary.destination == "ipn:0.1.77".parse().unwrap() {
            delta.route_key = Some("ipn:0.2.1".parse().unwrap());
        }
        Verdict::Continue(delta)
    }
}

// A bundle parks under the inputs its lookup walked: one addressed to an
// unregistered local service but keyed onto a foreign EID awaits a route to
// its key rather than its destination's service, so the key's route
// appearing forwards it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_keyed_local_destination_waits_for_its_key_route() {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let mut pack = FilterPack::new("test");
    pack.ingress_classifier("keyer", LocalKeyingClassifier);
    let bpa = Bpa::builder().node_ids(node_ids).add_filters(pack).build();
    bpa.start(false).await.unwrap();

    let (cla, forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();

    // A fresh arrival executes its routing decision before the CLA's
    // dispatch returns, so the bundle has parked by then, nothing routing
    // its key yet.
    let (bundle, data) = Builder::new("ipn:0.3.1".parse().unwrap(), "ipn:0.1.77".parse().unwrap())
        .with_payload(Cow::Borrowed(b"Keyed away".as_slice()))
        .build(CreationTimestamp::now())
        .expect("Failed to build bundle");
    assert_eq!(
        cla.sink
            .dispatch(None, None, &mut Bytes::from(data))
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // The key's route appears.
    cla.sink
        .add_peer(
            cla::Address::Private("peer".as_bytes().into()),
            &[NodeId::Ipn(IpnNodeId {
                allocator_id: 0,
                node_number: 2,
            })],
        )
        .await
        .unwrap();

    // Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout: the keyed bundle must wait for its key's route")
    .expect("Channel closed");
    let parsed = parse(forwarded).expect("Failed to parse forwarded bundle");
    assert_eq!(parsed.bundle.primary.id, bundle.primary.id);

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// A service whose `deliver` always returns `Err`
// ---------------------------------------------------------------------------

struct FailingService {
    sink: Arc<dyn Sink>,
    /// The endpoint the BPA settled on, which delivered bundles are
    /// addressed to.
    endpoint: Eid,
    /// Fires once `deliver` has built its `Err` and returned, so a waiter
    /// observes the point at which the dispatcher's error branch is about
    /// to run rather than the point at which `deliver` started.
    completed_tx: flume::Sender<()>,
}

impl FailingService {
    fn factory() -> (impl service::Factory<Component = Self>, flume::Receiver<()>) {
        let (tx, rx) = flume::bounded(16);
        (
            service::factory_fn(move |cx: service::Context| {
                Ok(Arc::new(Self {
                    sink: cx.sink,
                    endpoint: cx.endpoint,
                    completed_tx: tx,
                }))
            }),
            rx,
        )
    }
}

#[async_trait]
impl Service for FailingService {
    async fn deliver(
        &self,
        _bundle_id: &Id,
        _expiry: time::OffsetDateTime,
        _ack_requested: bool,
        _total_len: u64,
        _stream: &mut dyn Receiver<Segment>,
    ) -> service::Result<()> {
        let err = service::Error::Internal("test: simulated delivery failure".into());
        let _ = self.completed_tx.send(());
        Err(err)
    }

    async fn report_status(
        &self,
        _bundle_id: &Id,
        _from: &Eid,
        _kind: StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<time::OffsetDateTime>,
    ) {
    }
}

// ---------------------------------------------------------------------------
// INT-BPA-05: dispatcher tolerates deliver returning Err
// ---------------------------------------------------------------------------

/// When `deliver` returns `Err`, the dispatcher keeps the bundle as
/// `WaitingForService` rather than reporting it delivered and deleting it.
/// After the failing receiver rejects the bundle, a working receiver
/// registered on the same service id receives it. A regression to dropping
/// on error makes the re-delivery time out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatcher_handles_deliver_err() {
    let bpa = Bpa::builder().build();
    bpa.start(false).await.unwrap();

    let (factory, _sender_rx) = TestService::factory();
    let sender = bpa
        .register_service(Some(ServiceId::Ipn(43)), factory)
        .await
        .unwrap();

    let (factory, completed_rx) = FailingService::factory();
    let failing = bpa
        .register_service(Some(ServiceId::Ipn(42)), factory)
        .await
        .unwrap();
    let receiver_eid = failing.endpoint.clone();

    sender
        .sink
        .send(
            receiver_eid,
            Bytes::from_static(b"payload"),
            Duration::from_secs(3600),
            None,
        )
        .await
        .unwrap();

    // The failing receiver rejects the bundle from within `deliver`.
    // Event-driven wait; the timeout only bounds a regression.
    tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        completed_rx.recv_async(),
    )
    .await
    .expect("dispatcher must call deliver")
    .unwrap();

    // A working receiver on the same service id, with no quiesce needed:
    // the failed delivery's park re-checks the routing snapshot, so whether
    // it lands before this registration's `WaitingForService` poll or after
    // it, the bundle re-enters dispatch and reaches the new receiver.
    failing.sink.unregister().await;
    let (factory, received_rx) = TestService::factory();
    bpa.register_service(Some(ServiceId::Ipn(42)), factory)
        .await
        .unwrap();

    // Event-driven wait; the timeout only bounds a regression.
    let (_source, payload) = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        received_rx.recv_async(),
    )
    .await
    .expect("parked bundle must be re-delivered on re-registration")
    .unwrap();
    assert_eq!(payload, Bytes::from_static(b"payload"));

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// A CLA that answers `Accepted` to the first N offers and `Sent` afterwards,
// reporting every offered bundle id to the test
// ---------------------------------------------------------------------------

struct DeferringCla {
    sink: Arc<dyn cla::Sink>,
    offers_tx: flume::Sender<Id>,
    remaining_accepts: AtomicUsize,
}

impl DeferringCla {
    fn factory(accepts: usize) -> (impl Factory<Component = Self>, flume::Receiver<Id>) {
        let (tx, rx) = flume::bounded(16);
        (
            factory_fn(Request::default(), move |cx: Context| {
                Ok(Arc::new(Self {
                    sink: cx.sink,
                    offers_tx: tx,
                    remaining_accepts: AtomicUsize::new(accepts),
                }))
            }),
            rx,
        )
    }

    fn sink(&self) -> &dyn cla::Sink {
        self.sink.as_ref()
    }
}

#[async_trait]
impl cla::Cla for DeferringCla {
    async fn forward(
        &self,
        _lane: Option<u32>,
        _cla_addr: &cla::Address,
        bundle_id: &Id,
        _total_len: u64,
        _stream: &mut dyn Receiver<cla::Segment>,
    ) -> cla::Result<cla::ForwardOutcome> {
        let _ = self.offers_tx.send(bundle_id.clone());
        if self
            .remaining_accepts
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            Ok(cla::ForwardOutcome::Accepted)
        } else {
            Ok(cla::ForwardOutcome::Sent)
        }
    }
}

// A BPA with a `DeferringCla` registered and a peer for ipn:0.<node>.
async fn deferring_setup(
    accepts: usize,
    peer_node_number: u32,
) -> (hardy_bpa::bpa::Bpa, Arc<DeferringCla>, flume::Receiver<Id>) {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).build();
    bpa.start(false).await.unwrap();

    let (cla, offers_rx) = DeferringCla::factory(accepts);
    let cla = bpa
        .register_cla(&format!("deferring-{peer_node_number}"), cla)
        .await
        .unwrap();
    cla.sink()
        .add_peer(
            cla::Address::Private(format!("peer-{peer_node_number}").into_bytes().into()),
            &[NodeId::Ipn(IpnNodeId {
                allocator_id: 0,
                node_number: peer_node_number,
            })],
        )
        .await
        .unwrap();

    (bpa, cla, offers_rx)
}

async fn expect_offer(rx: &flume::Receiver<Id>) -> Id {
    // Event-driven wait; the timeout only bounds a regression.
    tokio::time::timeout(tokio::time::Duration::from_secs(5), rx.recv_async())
        .await
        .expect("Timeout waiting for CLA offer")
        .expect("Channel closed")
}

// Known test-guide deviation (quiet-window absence helper):
// scheduled for the dedicated pipeline de-flake pass (see bpa/docs/TODO.md).
async fn expect_no_offer(rx: &flume::Receiver<Id>) {
    assert!(
        tokio::time::timeout(tokio::time::Duration::from_secs(1), rx.recv_async())
            .await
            .is_err(),
        "Unexpected CLA offer"
    );
}

// ---------------------------------------------------------------------------
// INT-BPA-07: a deferred outcome of Failed re-enters dispatch, per bundle
// ---------------------------------------------------------------------------

/// A transfer answered `Accepted` whose outcome is reported `Failed` takes
/// a fresh routing decision and is re-offered to the CLA, the bundle never
/// being dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_outcome_failed_redispatches() {
    let (bpa, cla, offers_rx) = deferring_setup(1, 2).await;

    assert_eq!(
        cla.sink()
            .dispatch(
                None,
                None,
                &mut build_bundle(
                    &"ipn:0.3.1".parse().unwrap(),
                    &"ipn:0.2.99".parse().unwrap(),
                    b"deferred-fail",
                ),
            )
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    let id = expect_offer(&offers_rx).await;
    cla.sink()
        .report_transfer(&id, cla::TransferOutcome::Failed)
        .await
        .unwrap();

    // The failed transfer re-enters dispatch and is re-offered over the
    // unchanged route, and the CLA answers the second offer `Sent`.
    let id2 = expect_offer(&offers_rx).await;
    assert_eq!(id, id2, "Re-offer must be the same bundle");

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// INT-BPA-08: a deferred outcome of Completed resolves the transfer
// ---------------------------------------------------------------------------

/// A transfer answered `Accepted` whose outcome is reported `Completed` is
/// finished: there is no re-offer, a late duplicate outcome is ignored, and
/// a re-arrival of the same bundle is dropped as a duplicate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_outcome_completed_resolves() {
    let (bpa, cla, offers_rx) = deferring_setup(1, 2).await;

    let mut data = build_bundle(
        &"ipn:0.3.1".parse().unwrap(),
        &"ipn:0.2.99".parse().unwrap(),
        b"deferred-ok",
    );
    assert_eq!(
        cla.sink()
            .dispatch(None, None, &mut data.clone())
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    let id = expect_offer(&offers_rx).await;
    cla.sink()
        .report_transfer(&id, cla::TransferOutcome::Completed)
        .await
        .unwrap();

    // A late duplicate outcome is ignored rather than honoured twice.
    cla.sink()
        .report_transfer(&id, cla::TransferOutcome::Failed)
        .await
        .unwrap();
    expect_no_offer(&offers_rx).await;

    // The completed bundle's id was committed, so a re-arrival of the same
    // bundle is dropped as a duplicate rather than re-forwarded.
    assert_eq!(
        cla.sink().dispatch(None, None, &mut data).await.unwrap(),
        cla::Acceptance::Accepted
    );
    expect_no_offer(&offers_rx).await;

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// INT-BPA-09: removing a peer resolves a deferred outcome as unknown
// ---------------------------------------------------------------------------

/// Removing the peer while a transfer awaits its outcome resolves it as
/// unknown: the bundle returns to `Waiting` and is re-offered once the peer
/// comes back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_outcome_peer_removal_resolves_unknown() {
    let (bpa, cla, offers_rx) = deferring_setup(1, 2).await;

    assert_eq!(
        cla.sink()
            .dispatch(
                None,
                None,
                &mut build_bundle(
                    &"ipn:0.3.1".parse().unwrap(),
                    &"ipn:0.2.99".parse().unwrap(),
                    b"outcome-unknown",
                ),
            )
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    let id = expect_offer(&offers_rx).await;

    let peer_addr = cla::Address::Private("peer-2".as_bytes().into());
    let peer_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    assert!(cla.sink().remove_peer(&peer_addr).await.unwrap());
    assert!(
        cla.sink()
            .add_peer(peer_addr, core::slice::from_ref(&peer_node))
            .await
            .unwrap()
    );

    // The unresolved transfer returned to `Waiting` on the peer's removal,
    // and the re-added peer's route re-dispatches it.
    let id2 = expect_offer(&offers_rx).await;
    assert_eq!(id, id2, "Re-offer must be the same bundle");

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// INT-BPA-10: only the owning CLA resolves a transfer's deferred outcome
// ---------------------------------------------------------------------------

/// An outcome reported by a CLA that does not own the transfer's peer is
/// ignored, and the owning CLA's later outcome is still honoured. An
/// outcome for an unknown bundle is ignored without error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_outcome_ignores_wrong_cla() {
    let (bpa, cla_a, offers_a) = deferring_setup(1, 2).await;

    // A second CLA, with its own peer on another node.
    let (cla_b, offers_b) = DeferringCla::factory(0);
    let cla_b = bpa.register_cla("deferring-b", cla_b).await.unwrap();
    cla_b
        .sink()
        .add_peer(
            cla::Address::Private("peer-b".as_bytes().into()),
            &[NodeId::Ipn(IpnNodeId {
                allocator_id: 0,
                node_number: 4,
            })],
        )
        .await
        .unwrap();

    assert_eq!(
        cla_a
            .sink()
            .dispatch(
                None,
                None,
                &mut build_bundle(
                    &"ipn:0.3.1".parse().unwrap(),
                    &"ipn:0.2.99".parse().unwrap(),
                    b"wrong-cla",
                ),
            )
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    let id = expect_offer(&offers_a).await;

    // An outcome for a bundle the BPA has never seen is ignored without error.
    let unknown = Id {
        source: "ipn:0.9.9".parse().unwrap(),
        timestamp: CreationTimestamp::now(),
        fragment_info: None,
    };
    cla_b
        .sink()
        .report_transfer(&unknown, cla::TransferOutcome::Completed)
        .await
        .unwrap();

    // CLA B does not own the transfer's peer, so its outcome is ignored.
    cla_b
        .sink()
        .report_transfer(&id, cla::TransferOutcome::Completed)
        .await
        .unwrap();
    expect_no_offer(&offers_a).await;

    // The owning CLA's outcome is still honoured.
    cla_a
        .sink()
        .report_transfer(&id, cla::TransferOutcome::Failed)
        .await
        .unwrap();
    let id2 = expect_offer(&offers_a).await;
    assert_eq!(id, id2, "Re-offer must be the same bundle");
    expect_no_offer(&offers_b).await;

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// INT-BPA-11: expiry during a CLA-owned transfer lives in
// tests/forward_expiry.rs, where the reaper defers a `ForwardAckPending`
// bundle and the outcome resolves it, a failed transfer expiring at the
// dispatch expiry checkpoint.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// A CLA that holds the first forward open until released and then fails it
// synchronously, answering `Sent` to every later offer. Every offer is
// recorded.
// ---------------------------------------------------------------------------

struct BlockingCla {
    sink: Arc<dyn cla::Sink>,
    offered_tx: flume::Sender<Id>,
    release_rx: flume::Receiver<()>,
    first: AtomicBool,
}

impl BlockingCla {
    fn factory() -> (
        impl Factory<Component = Self>,
        flume::Receiver<Id>,
        flume::Sender<()>,
    ) {
        let (offered_tx, offered_rx) = flume::bounded(16);
        let (release_tx, release_rx) = flume::bounded(1);
        (
            factory_fn(Request::default(), move |cx: Context| {
                Ok(Arc::new(Self {
                    sink: cx.sink,
                    offered_tx,
                    release_rx,
                    first: AtomicBool::new(true),
                }))
            }),
            offered_rx,
            release_tx,
        )
    }
}

#[async_trait]
impl cla::Cla for BlockingCla {
    async fn forward(
        &self,
        _lane: Option<u32>,
        _cla_addr: &cla::Address,
        bundle_id: &Id,
        _total_len: u64,
        _stream: &mut dyn Receiver<Segment>,
    ) -> cla::Result<cla::ForwardOutcome> {
        let _ = self.offered_tx.send_async(bundle_id.clone()).await;
        if self.first.swap(false, Ordering::SeqCst) {
            let _ = self.release_rx.recv_async().await;
            return Err(cla::Error::StreamCancelled);
        }
        Ok(cla::ForwardOutcome::Sent)
    }
}

/// A route event that lands while a transfer is in flight is not missed.
/// The event's poll cannot see the bundle, claimed as `ForwardAckPending`,
/// but the synchronous failure's park re-checks the routing snapshot taken
/// when the flight began and re-enters dispatch, the only route to the
/// second offer, no further route events occurring.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forward_failure_park_recheck_redispatches() {
    let bpa = Bpa::builder().build();
    bpa.start(false).await.unwrap();

    let (cla, offered_rx, release_tx) = BlockingCla::factory();
    let cla = bpa.register_cla("blocking", cla).await.unwrap();
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .add_peer(
            cla::Address::Private("peer-a".as_bytes().into()),
            from_ref(&remote_node),
        )
        .await
        .unwrap();

    let (factory, _service_rx) = TestService::factory();
    let service = bpa
        .register_service(Some(ServiceId::Ipn(90)), factory)
        .await
        .unwrap();
    let id = service
        .sink
        .send(
            "ipn:0.2.7".parse().unwrap(),
            Bytes::from_static(b"recheck"),
            Duration::from_secs(3600),
            None,
        )
        .await
        .unwrap();

    // The transfer is in flight, held open by the CLA. Every timeout below
    // only bounds a regression.
    let offered =
        tokio::time::timeout(tokio::time::Duration::from_secs(5), offered_rx.recv_async())
            .await
            .expect("Timeout waiting for the first offer")
            .unwrap();
    assert_eq!(offered, id);

    // A second peer for the same node appears. Its route event polls
    // `Waiting` bundles and cannot see this one.
    cla.sink
        .add_peer(
            cla::Address::Private("peer-b".as_bytes().into()),
            from_ref(&remote_node),
        )
        .await
        .unwrap();

    // On release, the synchronous failure parks the bundle, and the park
    // detects the mid-flight routing change and re-dispatches.
    release_tx.send(()).expect("CLA gone");
    let offered =
        tokio::time::timeout(tokio::time::Duration::from_secs(5), offered_rx.recv_async())
            .await
            .expect("The park's recheck must re-dispatch the bundle")
            .unwrap();
    assert_eq!(offered, id);

    bpa.shutdown().await;
}

/// A synchronous forward failure does not resurrect a bundle resolved while
/// its transfer was in flight: the failure's `Waiting` park is conditional
/// and loses against the terminal claim.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forward_failure_never_resurrects_resolved_bundle() {
    let metadata_store = Arc::new(MetadataMemStorage::new(None));
    let bpa = Bpa::builder()
        .metadata_storage(metadata_store.clone())
        .build();
    bpa.start(false).await.unwrap();

    let (cla, offered_rx, release_tx) = BlockingCla::factory();
    let cla = bpa.register_cla("blocking", cla).await.unwrap();
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .add_peer(
            cla::Address::Private("peer-a".as_bytes().into()),
            from_ref(&remote_node),
        )
        .await
        .unwrap();

    let (factory, _service_rx) = TestService::factory();
    let service = bpa
        .register_service(Some(ServiceId::Ipn(91)), factory)
        .await
        .unwrap();
    let id = service
        .sink
        .send(
            "ipn:0.2.7".parse().unwrap(),
            Bytes::from_static(b"resolve me"),
            Duration::from_secs(3600),
            None,
        )
        .await
        .unwrap();

    // The transfer is in flight; the timeout only bounds a regression.
    let offered =
        tokio::time::timeout(tokio::time::Duration::from_secs(5), offered_rx.recv_async())
            .await
            .expect("Timeout waiting for the offer")
            .unwrap();
    assert_eq!(offered, id);

    // The bundle is resolved, as the reaper's terminal claim would leave
    // it, a delete winning every race.
    metadata_store.tombstone(&id).await.unwrap();

    // On release, the failure's park loses against the tombstone.
    // `shutdown` is the barrier: it joins the egress pollers, so the park
    // has run by the time it returns, with no quiet window involved.
    release_tx.send(()).expect("CLA gone");
    bpa.shutdown().await;

    assert!(
        metadata_store.get(&id).await.unwrap().is_none(),
        "a resolved bundle was resurrected by a failed transfer's park"
    );
    let (live_tx, live_rx) = hardy_async::channel::bounded(4);
    metadata_store.poll_waiting(&live_tx).await.unwrap();
    drop(live_tx);
    assert!(
        live_rx.recv().await.is_err(),
        "a resolved bundle re-entered Waiting"
    );
}

// ---------------------------------------------------------------------------
// The config-gated RFC 9171 validity checks, applied before the drain
// ---------------------------------------------------------------------------

/// Feeds `data` to the BPA through the CLA sink as one Final segment.
async fn dispatch_inbound(cla: &PipelineCla, data: Bytes) {
    let (tx, mut rx) = hardy_async::channel::bounded(1);
    let producer = tokio::spawn(async move {
        hardy_async::channel::Sender::send(&tx, Segment::Final(data))
            .await
            .unwrap();
    });
    assert_eq!(
        cla.sink.dispatch(None, None, &mut rx).await.unwrap(),
        cla::Acceptance::Accepted
    );
    producer.await.unwrap();
}

/// Builds a strict-or-relaxed BPA around a local service at ipn:0.1.42,
/// returning the delivery channel.
async fn gate_fixture(
    configure: impl FnOnce(hardy_bpa::builder::BpaBuilder) -> hardy_bpa::builder::BpaBuilder,
) -> (Bpa, Arc<PipelineCla>, flume::Receiver<(Eid, Bytes)>) {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let bpa = configure(Bpa::builder().node_ids(node_ids)).build();
    bpa.start(false).await.unwrap();

    let (factory, service_rx) = TestService::factory();
    bpa.register_service(Some(ServiceId::Ipn(42)), factory)
        .await
        .unwrap();

    let (cla, _forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();

    (bpa, cla, service_rx)
}

fn unprotected_primary_bundle() -> Bytes {
    let (_, data) = Builder::new("ipn:0.2.1".parse().unwrap(), "ipn:0.1.42".parse().unwrap())
        .with_crc_type(hardy_bpv7::crc::CrcType::None)
        .with_payload(Cow::Borrowed(b"no integrity".as_slice()))
        .build(CreationTimestamp::now())
        .expect("Failed to build bundle");
    Bytes::from(data)
}

fn clockless_ageless_bundle() -> Bytes {
    let (_, data) = Builder::new("ipn:0.2.1".parse().unwrap(), "ipn:0.1.42".parse().unwrap())
        .with_payload(Cow::Borrowed(b"no clock".as_slice()))
        .build(CreationTimestamp::default())
        .expect("Failed to build bundle");
    Bytes::from(data)
}

/// RFC 9171 Section 4.3.1: with the default strict config, a primary block
/// with neither CRC nor BIB coverage is rejected at the pre-drain gate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_rejects_unprotected_primary_block() {
    let (bpa, cla, service_rx) = gate_fixture(|b| b).await;

    dispatch_inbound(&cla, unprotected_primary_bundle()).await;

    // Shutdown is the barrier: an admitted bundle would have completed
    // delivery before it returns.
    bpa.shutdown().await;
    assert!(
        service_rx.is_empty(),
        "an unprotected primary block must be rejected at the gate"
    );
}

/// `primary_block_integrity(false)` relaxes the Section 4.3.1 check: the
/// same bundle is admitted and delivered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relaxed_gate_admits_unprotected_primary_block() {
    let (bpa, cla, service_rx) = gate_fixture(|b| b.primary_block_integrity(false)).await;

    dispatch_inbound(&cla, unprotected_primary_bundle()).await;

    // Event-driven wait; the timeout only bounds a regression.
    let (_, payload) =
        tokio::time::timeout(tokio::time::Duration::from_secs(5), service_rx.recv_async())
            .await
            .expect("Timeout waiting for delivery")
            .expect("Channel closed");
    assert_eq!(payload.as_ref(), b"no integrity");

    bpa.shutdown().await;
}

/// RFC 9171 Section 4.4.2: with the default strict config, a clockless
/// bundle without a Bundle Age block is rejected at the pre-drain gate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_rejects_clockless_bundle_without_age() {
    let (bpa, cla, service_rx) = gate_fixture(|b| b).await;

    dispatch_inbound(&cla, clockless_ageless_bundle()).await;

    // Shutdown is the barrier: an admitted bundle would have completed
    // delivery before it returns.
    bpa.shutdown().await;
    assert!(
        service_rx.is_empty(),
        "a clockless bundle without a Bundle Age block must be rejected at the gate"
    );
}

/// `bundle_age_required(false)` relaxes the Section 4.4.2 check: the same
/// bundle is admitted and delivered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relaxed_gate_admits_clockless_bundle_without_age() {
    let (bpa, cla, service_rx) = gate_fixture(|b| b.bundle_age_required(false)).await;

    dispatch_inbound(&cla, clockless_ageless_bundle()).await;

    // Event-driven wait; the timeout only bounds a regression.
    let (_, payload) =
        tokio::time::timeout(tokio::time::Duration::from_secs(5), service_rx.recv_async())
            .await
            .expect("Timeout waiting for delivery")
            .expect("Channel closed");
    assert_eq!(payload.as_ref(), b"no clock");

    bpa.shutdown().await;
}

// A peer removed while its construction is still in flight leaves nothing
// behind: `add_peer` claims the address, builds the peer, publishes it, then
// re-checks its claim before installing RIB entries, so a concurrent
// `remove_peer` wins the claim and the half-added peer is withdrawn
// (`add_peer` returns false). Controller construction is the interception
// point: it parks until the test releases it, and a fresh add on the same
// address afterwards succeeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peer_removed_mid_construction_is_withdrawn() {
    use hardy_bpa::policy;

    struct ParkedPolicy {
        entered_tx: flume::Sender<()>,
        release_rx: flume::Receiver<()>,
    }

    struct DefaultController {
        queue: Arc<dyn policy::EgressQueue>,
    }

    #[async_trait]
    impl policy::FlowController for DefaultController {
        fn queue_for(&self) -> u32 {
            0
        }

        async fn forward(&self, _queue: u32, bundle: hardy_bpa::bundle::Bundle) {
            self.queue.forward(bundle).await
        }
    }

    #[async_trait]
    impl policy::FlowControllerFactory for ParkedPolicy {
        fn queue_count(&self) -> NonZeroU32 {
            NonZeroU32::MIN
        }

        async fn new_controller(
            &self,
            queues: policy::EgressQueueSet,
        ) -> Arc<dyn policy::FlowController> {
            let _ = self.entered_tx.send(());
            // Parked until the test drops the release sender; the peer is
            // mid-construction for exactly this window.
            let _ = self.release_rx.recv_async().await;
            Arc::new(DefaultController {
                queue: queues.next_free,
            })
        }
    }

    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let (entered_tx, entered_rx) = flume::unbounded();
    let (release_tx, release_rx) = flume::bounded::<()>(1);
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .egress_policy(
            "test",
            Arc::new(ParkedPolicy {
                entered_tx,
                release_rx,
            }),
        )
        .build();
    bpa.start(false).await.unwrap();

    let (cla, _forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();

    let peer_addr = cla::Address::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });

    // The add parks in controller construction...
    let add = {
        let cla = cla.clone();
        let peer_addr = peer_addr.clone();
        let remote_node = remote_node.clone();
        tokio::spawn(async move { cla.sink.add_peer(peer_addr, from_ref(&remote_node)).await })
    };
    // Event-driven wait; the timeout only bounds a regression.
    tokio::time::timeout(tokio::time::Duration::from_secs(5), entered_rx.recv_async())
        .await
        .expect("Timeout waiting for controller construction to start")
        .expect("Policy gone");

    // ...and the removal wins the claim while it is parked.
    assert!(
        cla.sink
            .remove_peer(&peer_addr)
            .await
            .expect("remove_peer failed"),
        "the claimed address is removable mid-construction"
    );

    drop(release_tx);
    let added = add
        .await
        .expect("add task panicked")
        .expect("add_peer failed");
    assert!(!added, "a removed claim must not complete as an added peer");

    // The address is free and a fresh add succeeds (the release sender is
    // dropped, so its construction completes immediately).
    assert!(
        cla.sink
            .add_peer(peer_addr, from_ref(&remote_node))
            .await
            .expect("add_peer failed"),
        "a fresh add on the freed address succeeds"
    );

    bpa.shutdown().await;
}

// Routing is decided at the ingress gate: a destination resolving to an
// explicit Drop route rejects the bundle before its payload is drained, so
// the stream is left unconsumed and nothing spools. A Drop carrying a reason
// reports as any other gated drop does, with one combined reception and
// deletion record; a reasonless Drop emits nothing at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn early_route_drop_never_awaits_the_payload() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .status_reports(true)
        .build();
    bpa.start(false).await.unwrap();

    // A peer for the report route (report-to defaults to the source), plus
    // Drop routes: reasoned for one destination, silent for another.
    let (cla, forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();
    let peer_addr = cla::Address::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();
    bpa.register_routing_agent(
        "drop-routes",
        StaticAgent::factory(&[
            (
                "ipn:0.9.99".parse().unwrap(),
                RouteAction::Drop(Some(ReasonCode::NoKnownRouteToDestinationFromHere)),
                1,
            ),
            ("ipn:0.9.98".parse().unwrap(), RouteAction::Drop(None), 1),
        ]),
    )
    .await
    .unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let oversized = |dest: &str| {
        let (_, data) = Builder::new(remote_source.clone(), dest.parse().unwrap())
            .with_flags(Flags {
                receipt_report_requested: true,
                delete_report_requested: true,
                ..Default::default()
            })
            .with_payload(Cow::Owned(vec![0x5A_u8; 50_000]))
            .build(CreationTimestamp::now())
            .expect("Failed to build bundle");
        data
    };

    // Reasoned Drop: accepted with one combined report while the payload
    // source is parked forever, so the verdict returning at all proves the
    // decision never awaited the drain. The timeout only bounds a
    // regression.
    let data = oversized("ipn:0.9.99");
    let mut stream = ParkedReceiver::new(&data, 1000, 3);
    assert_eq!(
        tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            cla.sink.dispatch(Some(&remote_node), None, &mut stream)
        )
        .await
        .expect("Timeout: the route drop must not wait for the payload")
        .unwrap(),
        cla::Acceptance::Accepted,
        "a route-dropped bundle is accepted and terminated, never refused"
    );
    // Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for the status report")
    .expect("Channel closed");
    let parsed = parse(forwarded).expect("Failed to parse forwarded bundle");
    assert!(parsed.bundle.primary.flags.is_admin_record);
    assert_eq!(parsed.bundle.primary.destination, remote_source);
    let body = parsed
        .bundle
        .blocks
        .get(&1)
        .expect("report has a payload block")
        .payload(&parsed.data)
        .expect("report payload in bundle");
    let AdministrativeRecord::BundleStatusReport(status) =
        hardy_cbor::decode::parse(body).expect("report payload is an admin record");
    assert!(status.received.is_some(), "reception asserted");
    assert!(status.deleted.is_some(), "deletion asserted");
    assert_eq!(status.reason, ReasonCode::NoKnownRouteToDestinationFromHere);

    // Silent Drop: accepted while the source is parked, no report at all.
    let data = oversized("ipn:0.9.98");
    let mut stream = ParkedReceiver::new(&data, 1000, 3);
    assert_eq!(
        tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            cla.sink.dispatch(Some(&remote_node), None, &mut stream)
        )
        .await
        .expect("Timeout: the silent route drop must not wait for the payload")
        .unwrap(),
        cla::Acceptance::Accepted
    );

    // The completed shutdown is the barrier proving the absence.
    bpa.shutdown().await;
    assert!(
        forwarded_rx.is_empty(),
        "a reasonless Drop route emits nothing"
    );
}

/// A routing agent that hands its sink to the test, which installs and
/// withdraws routes at chosen moments.
struct SinkAgent {
    sink: Arc<dyn routing::Sink>,
}

impl SinkAgent {
    fn factory() -> impl routing::Factory<Component = Self> {
        routing::factory_fn(|cx: routing::Context| Ok(Arc::new(Self { sink: cx.sink })))
    }
}

impl Agent for SinkAgent {}

// The gate's decision of record is made again at the commit when the routing
// table changed during the drain, so a route withdrawn while the payload was
// still arriving does not carry the bundle. The route moves from one peer to
// another while the source is parked mid-payload, and the bundle leaves
// through the new route's peer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_route_changed_during_the_drain_is_decided_again() {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).build();
    bpa.start(false).await.unwrap();

    // Two CLAs with one peer each: ipn:0.2 behind the first, ipn:0.4 behind
    // the second.
    let (old_cla, old_rx) = PipelineCla::factory();
    let old_cla = bpa.register_cla("old", old_cla).await.unwrap();
    old_cla
        .sink
        .add_peer(
            cla::Address::Private("old".as_bytes().into()),
            &[NodeId::Ipn(IpnNodeId {
                allocator_id: 0,
                node_number: 2,
            })],
        )
        .await
        .unwrap();
    let (new_cla, new_rx) = PipelineCla::factory();
    let new_cla = bpa.register_cla("new", new_cla).await.unwrap();
    new_cla
        .sink
        .add_peer(
            cla::Address::Private("new".as_bytes().into()),
            &[NodeId::Ipn(IpnNodeId {
                allocator_id: 0,
                node_number: 4,
            })],
        )
        .await
        .unwrap();

    let agent = bpa
        .register_routing_agent("swap", SinkAgent::factory())
        .await
        .unwrap();
    let routes = &agent.sink;
    let old_via = RouteAction::Via("ipn:0.2.0".parse().unwrap());
    let new_via = RouteAction::Via("ipn:0.4.0".parse().unwrap());
    assert!(
        routes
            .add_route("ipn:0.3.*".parse().unwrap(), old_via.clone(), 1)
            .await
            .unwrap()
    );

    let (bundle, data) = Builder::new("ipn:0.2.1".parse().unwrap(), "ipn:0.3.1".parse().unwrap())
        .with_payload(Cow::Owned(vec![0x5A_u8; 50_000]))
        .build(CreationTimestamp::now())
        .expect("Failed to build bundle");
    let data = Bytes::from(data);
    let start = usize::try_from(bundle.blocks[&1].payload_range().start).unwrap();

    // Each send on the one-slot channel completes only once the previous
    // segment has been pulled. The header pass takes the head alone (no
    // filter declares a peek), so the drain pulling the first payload
    // segment proves the gate's lookup is done.
    let (tx, mut rx) = hardy_async::channel::bounded(1);
    let arrival = old_cla.clone();
    let dispatch = tokio::spawn(async move { arrival.sink.dispatch(None, None, &mut rx).await });
    tx.send(Segment::Next(data.slice(..start))).await.unwrap();
    tx.send(Segment::Next(data.slice(start..start + 1000)))
        .await
        .unwrap();
    tx.send(Segment::Next(data.slice(start + 1000..start + 2000)))
        .await
        .unwrap();

    // The gate decided on the old route; move it to the new peer while the
    // payload is still arriving.
    assert!(
        routes
            .add_route("ipn:0.3.*".parse().unwrap(), new_via, 1)
            .await
            .unwrap()
    );
    assert!(
        routes
            .remove_route(&"ipn:0.3.*".parse().unwrap(), &old_via, 1)
            .await
            .unwrap()
    );
    tx.send(Segment::Final(data.slice(start + 2000..)))
        .await
        .unwrap();
    drop(tx);

    // The timeout only bounds a regression.
    assert_eq!(
        tokio::time::timeout(tokio::time::Duration::from_secs(5), dispatch)
            .await
            .expect("Timeout waiting for the arrival to settle")
            .unwrap()
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(tokio::time::Duration::from_secs(5), new_rx.recv_async())
        .await
        .expect("Timeout: the bundle must leave by the route in force at the commit")
        .expect("Channel closed");
    let parsed = parse(forwarded).expect("Failed to parse forwarded bundle");
    assert_eq!(parsed.bundle.primary.id, bundle.primary.id);

    // The completed shutdown is the barrier proving the absence.
    bpa.shutdown().await;
    assert!(old_rx.is_empty(), "the withdrawn route carries nothing");
}

/// A service whose status notifications block until the test releases
/// them, announcing each as it begins.
struct BlockingNotifyApp {
    entered_tx: flume::Sender<()>,
    release_rx: flume::Receiver<()>,
}

#[async_trait]
impl Service for BlockingNotifyApp {
    async fn deliver(
        &self,
        _bundle_id: &Id,
        _expiry: time::OffsetDateTime,
        _ack_requested: bool,
        _total_len: u64,
        _stream: &mut dyn Receiver<Segment>,
    ) -> service::Result<()> {
        Ok(())
    }

    async fn report_status(
        &self,
        _bundle_id: &Id,
        _from: &Eid,
        _kind: StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<time::OffsetDateTime>,
    ) {
        let _ = self.entered_tx.send(());
        let _ = self.release_rx.recv_async().await;
    }
}

// An arrival's admin-endpoint arm runs on the processing pool, not on the
// arriving CLA's task: the embedder callbacks it makes do not hold the
// session's acknowledgement. The CLA's dispatch returns while the status
// notification is still blocked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_admin_record_does_not_hold_the_arriving_cla() {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).build();
    bpa.start(false).await.unwrap();

    let (entered_tx, entered_rx) = flume::bounded(1);
    let (release_tx, release_rx) = flume::bounded(1);
    bpa.register_service(
        Some(ServiceId::Ipn(42)),
        service::factory_fn(move |_| {
            Ok(Arc::new(BlockingNotifyApp {
                entered_tx,
                release_rx,
            }))
        }),
    )
    .await
    .unwrap();
    let (cla, _forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();

    // A delivery report about a bundle the service sent, from a remote node's
    // admin endpoint to the local one.
    let (record, ()) = emit(&AdministrativeRecord::BundleStatusReport(
        BundleStatusReport {
            bundle_id: Id {
                source: "ipn:0.1.42".parse().unwrap(),
                timestamp: CreationTimestamp::now(),
                fragment_info: None,
            },
            delivered: Some(StatusAssertion(None)),
            ..Default::default()
        },
    ));
    let (_, data) = Builder::new("ipn:0.2.0".parse().unwrap(), "ipn:0.1.0".parse().unwrap())
        .with_flags(Flags {
            is_admin_record: true,
            ..Default::default()
        })
        .with_payload(Cow::Owned(record))
        .build(CreationTimestamp::now())
        .expect("Failed to build bundle");

    // The timeout only bounds a regression.
    assert_eq!(
        tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            cla.sink.dispatch(None, None, &mut Bytes::from(data))
        )
        .await
        .expect("Timeout: the admin record must not hold the arriving CLA")
        .unwrap(),
        cla::Acceptance::Accepted
    );

    // Event-driven wait; the timeout only bounds a regression.
    tokio::time::timeout(tokio::time::Duration::from_secs(5), entered_rx.recv_async())
        .await
        .expect("Timeout waiting for the status notification")
        .expect("Channel closed");
    release_tx.send(()).unwrap();

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// Origination: the validation split, the route lookup, and the caller's
// error surface
// ---------------------------------------------------------------------------

/// Routing is decided before the payload drains: a destination resolving to
/// an explicit Drop route rejects the send with the stream left unconsumed
/// and nothing spooled. Unlike CLA ingress there is never a status report,
/// report flags notwithstanding: the error return is the originator's
/// notification, and even a reasonless Drop route surfaces as
/// `Dropped(None)`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn originate_route_drop_never_awaits_the_payload() {
    let (bpa, svc, forwarded_rx, source_eid) = streamed_originate_setup().await;
    bpa.register_routing_agent(
        "drop-routes",
        StaticAgent::factory(&[
            (
                "ipn:0.9.99".parse().unwrap(),
                RouteAction::Drop(Some(ReasonCode::NoKnownRouteToDestinationFromHere)),
                1,
            ),
            ("ipn:0.9.98".parse().unwrap(), RouteAction::Drop(None), 1),
        ]),
    )
    .await
    .unwrap();

    let oversized = |dest: &str| {
        let (_, data) = Builder::new(source_eid.clone(), dest.parse().unwrap())
            .with_flags(Flags {
                receipt_report_requested: true,
                delete_report_requested: true,
                ..Default::default()
            })
            .with_payload(Cow::Owned(vec![0x5A_u8; 50_000]))
            .build(CreationTimestamp::now())
            .expect("Failed to build bundle");
        data
    };

    // Reasoned Drop: the error returns while the payload source is parked
    // forever, so returning at all proves the decision never awaited the
    // drain. The timeout only bounds a regression.
    let data = oversized("ipn:0.9.99");
    let mut stream = ParkedReceiver::new(&data, 1000, 3);
    let result = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        svc.sink.send(&mut stream),
    )
    .await
    .expect("Timeout: the route drop must not wait for the payload");
    assert!(matches!(
        result,
        Err(service::Error::Dropped(Some(
            ReasonCode::NoKnownRouteToDestinationFromHere
        )))
    ));

    // Silent Drop: the originator still gets an error, though CLA ingress
    // drops silently.
    let data = oversized("ipn:0.9.98");
    let mut stream = ParkedReceiver::new(&data, 1000, 3);
    let result = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        svc.sink.send(&mut stream),
    )
    .await
    .expect("Timeout: the silent route drop must not wait for the payload");
    assert!(matches!(result, Err(service::Error::Dropped(None))));

    // The completed shutdown is the barrier proving the absence.
    bpa.shutdown().await;
    assert!(
        forwarded_rx.is_empty(),
        "an originate rejection must not emit status reports"
    );
}

// `Sink::send` and `Sink::send_streamed` refuse a bundle that expires at
// creation, as `raw::Sink::send`'s admission gate does: a zero lifetime
// surfaces as `Dropped(LifetimeExpired)` with nothing admitted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adu_originate_refuses_a_zero_lifetime() {
    let (bpa, _svc, forwarded_rx, _) = streamed_originate_setup().await;
    let (factory, _service_rx) = TestService::factory();
    let service = bpa
        .register_service(Some(ServiceId::Ipn(42)), factory)
        .await
        .unwrap();

    let result = service
        .sink
        .send(
            "ipn:0.2.1".parse().unwrap(),
            Bytes::from_static(b"expired"),
            Duration::ZERO,
            None,
        )
        .await;
    assert!(matches!(
        result,
        Err(service::Error::Dropped(Some(ReasonCode::LifetimeExpired)))
    ));

    // `send_streamed`, the same refusal before the payload is pulled: the
    // source pends forever if pulled. The timeout only bounds a regression.
    let mut stream = ParkedReceiver::new(b"expired", 7, 0);
    let result = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        service.sink.send_streamed(
            "ipn:0.2.1".parse().unwrap(),
            7,
            &mut stream,
            Duration::ZERO,
            None,
        ),
    )
    .await
    .expect("Timeout: the expired send must not pull the payload");
    assert!(matches!(
        result,
        Err(service::Error::Dropped(Some(ReasonCode::LifetimeExpired)))
    ));

    // The completed shutdown is the barrier proving the absence.
    bpa.shutdown().await;
    assert!(forwarded_rx.is_empty(), "nothing was admitted");
}

/// `raw::Sink::send` carries the validation the ADU sinks do not:
/// hop-exhausted service bytes trip the lifetime and hop admission check,
/// and the config-gated RFC 9171 validity checks, strict by default, apply
/// to raw sends as well. Each surfaces as `Dropped(reason)` to the caller,
/// with nothing admitted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_originate_applies_the_admission_gates() {
    let (bpa, svc, forwarded_rx, source_eid) = streamed_originate_setup().await;
    let dest: Eid = "ipn:0.2.1".parse().unwrap();

    // Hop-exhausted, the count past the limit.
    let (_, data) = Builder::new(source_eid.clone(), dest.clone())
        .with_hop_count(&HopInfo {
            limit: NonZeroU8::new(1).unwrap(),
            count: 2,
        })
        .with_payload(Cow::Borrowed(b"exhausted".as_slice()))
        .build(CreationTimestamp::now())
        .expect("Failed to build bundle");
    let result = svc.sink.send(&mut Bytes::from(data)).await;
    assert!(matches!(
        result,
        Err(service::Error::Dropped(Some(ReasonCode::HopLimitExceeded)))
    ));

    // Unprotected primary block: RFC 9171 Section 4.3.1.
    let (_, data) = Builder::new(source_eid.clone(), dest.clone())
        .with_crc_type(CrcType::None)
        .with_payload(Cow::Borrowed(b"no integrity".as_slice()))
        .build(CreationTimestamp::now())
        .expect("Failed to build bundle");
    let result = svc.sink.send(&mut Bytes::from(data)).await;
    assert!(matches!(
        result,
        Err(service::Error::Dropped(Some(
            ReasonCode::BlockUnintelligible
        )))
    ));

    // Clockless without a Bundle Age block: RFC 9171 Section 4.4.2.
    let (_, data) = Builder::new(source_eid.clone(), dest)
        .with_payload(Cow::Borrowed(b"no clock".as_slice()))
        .build(CreationTimestamp::default())
        .expect("Failed to build bundle");
    let result = svc.sink.send(&mut Bytes::from(data)).await;
    assert!(matches!(
        result,
        Err(service::Error::Dropped(Some(ReasonCode::LifetimeExpired)))
    ));

    // The completed shutdown is the barrier proving the absence.
    bpa.shutdown().await;
    assert!(forwarded_rx.is_empty(), "nothing was admitted");
}

/// A service-built fragment is rejected: fragmentation is a forwarding-time
/// action (RFC 9171 Section 5.8), so a source never emits fragments. The
/// send surfaces `FragmentedBundle` and nothing is admitted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_originate_rejects_a_fragment() {
    let (bpa, svc, forwarded_rx, source_eid) = streamed_originate_setup().await;
    let dest: Eid = "ipn:0.2.1".parse().unwrap();

    // A valid bundle, rewritten as a self-declared fragment: the editor
    // sets the fragment flag from the fragment info and re-emits canonical
    // bytes, so only the fragment fact can trip the rejection.
    let (_, data) = Builder::new(source_eid, dest)
        .with_payload(Cow::Borrowed(b"a piece of an adu".as_slice()))
        .build(CreationTimestamp::now())
        .expect("Failed to build bundle");
    let Parsed {
        bundle: raw, data, ..
    } = parse(Bytes::from(data)).expect("parse the built bundle");
    let editor = Editor::new(&raw, &data)
        .with_fragment_info(Some(FragmentInfo {
            offset: 0,
            total_adu_length: 1024,
        }))
        .map_err(|(_, e)| e)
        .expect("set fragment info");
    let chunks = editor.rebuild().expect("rebuild the fragment");
    let mut fragment = Chunk::flatten_bytes(chunks, data);

    let result = svc.sink.send(&mut fragment).await;
    assert!(matches!(result, Err(service::Error::FragmentedBundle)));

    // The completed shutdown is the barrier proving the absence.
    bpa.shutdown().await;
    assert!(forwarded_rx.is_empty(), "the fragment must not be admitted");
}

/// A duplicate raw send surfaces `DuplicateBundle` to the caller, since
/// service bundles carry service-authored ids that the BPA cannot reissue
/// for a retry. The first copy is unaffected, forwarded exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_originate_duplicate_surfaces_to_the_caller() {
    let (bpa, svc, forwarded_rx, source_eid) = streamed_originate_setup().await;

    let dest: Eid = "ipn:0.2.1".parse().unwrap();
    let data = build_bundle(&source_eid, &dest, b"send once");

    let mut first = data.clone();
    let id = svc.sink.send(&mut first).await.expect("first send admits");

    // Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for forwarded bundle")
    .expect("Channel closed");
    assert_eq!(
        parse(forwarded).expect("parse forwarded").bundle.primary.id,
        id
    );

    let mut second = data;
    let result = svc.sink.send(&mut second).await;
    assert!(matches!(result, Err(service::Error::DuplicateBundle)));

    // The completed shutdown is the barrier proving the absence.
    bpa.shutdown().await;
    assert!(forwarded_rx.is_empty(), "the duplicate must not forward");
}

// The originate twin of the gate-side duplicate probe: once the first send
// commits, a replay is refused with `DuplicateBundle` while its payload
// source is still parked, so the caller's stream is never drained for a
// bundle the node already holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_originate_duplicate_never_awaits_the_payload() {
    let (bpa, svc, forwarded_rx, source_eid) = streamed_originate_setup().await;

    let dest: Eid = "ipn:0.2.1".parse().unwrap();
    // Oversized payload so the replay genuinely streams (headers < payload).
    let payload = vec![0x5A_u8; 20_000];
    let data = build_bundle(&source_eid, &dest, &payload);

    let mut first = data.clone();
    svc.sink.send(&mut first).await.expect("first send admits");
    // Event-driven wait; the timeout only bounds a regression.
    tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for forwarded bundle")
    .expect("Channel closed");

    // Replay with the payload parked: the duplicate must surface from the
    // gate probe, not from an insert that waited on the whole payload. The
    // timeout only bounds a regression, since an origination that waits for
    // the payload parks forever.
    let mut replay = ParkedReceiver::new(&data, 1000, 3);
    let result = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        svc.sink.send(&mut replay),
    )
    .await
    .expect("Timeout: the duplicate origination awaited its parked payload");
    assert!(
        matches!(result, Err(service::Error::DuplicateBundle)),
        "the parked replay must surface DuplicateBundle without the payload"
    );

    bpa.shutdown().await;
    assert!(forwarded_rx.is_empty(), "the duplicate must not forward");
}

// Builds a node over the supplied storage, with a peer for ipn:0.2.
async fn restart_setup(
    metadata: Arc<MetadataMemStorage>,
    bundles: Arc<BundleMemStorage>,
) -> (
    Bpa,
    Arc<EchoService>,
    Arc<PipelineCla>,
    flume::Receiver<Bytes>,
) {
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
        .metadata_storage(metadata)
        .bundle_storage(bundles)
        .build();
    bpa.start(false).await.unwrap();
    let svc = bpa
        .register_raw_service(Some(ServiceId::Ipn(7)), EchoService::factory())
        .await
        .unwrap();
    let (cla, forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();
    cla.sink
        .add_peer(
            cla::Address::Private("peer".as_bytes().into()),
            &[NodeId::Ipn(IpnNodeId {
                allocator_id: 0,
                node_number: 2,
            })],
        )
        .await
        .unwrap();
    (bpa, svc, cla, forwarded_rx)
}

// The number of bundles `bundles` holds.
async fn stored_bundles(bundles: &BundleMemStorage) -> usize {
    let (tx, rx) = hardy_async::channel::unbounded();
    bundles.recover(&tx).await.unwrap();
    drop(tx);
    let mut count = 0;
    while rx.recv().await.is_ok() {
        count += 1;
    }
    count
}

// The duplicate refusal of record is the metadata insert; the recent-id
// cache answers only for what this process committed. Bundles forwarded
// before a restart arrive again after it, the cache now cold: a CLA arrival
// is accepted and refused at the insert, a raw send surfaces
// `DuplicateBundle`, neither is forwarded again, and neither copy's spooled
// data is kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_duplicate_after_a_restart_is_refused_at_the_insert() {
    let metadata = Arc::new(MetadataMemStorage::new(None));
    let bundles = Arc::new(BundleMemStorage::new(None, None));
    let dest: Eid = "ipn:0.2.1".parse().unwrap();
    let arrival = build_bundle(&"ipn:0.3.1".parse().unwrap(), &dest, b"arrives once");
    let raw = build_bundle(&"ipn:0.1.7".parse().unwrap(), &dest, b"sent once");

    // First life: both forward. The timeouts only bound a regression.
    let (bpa, svc, cla, forwarded_rx) = restart_setup(metadata.clone(), bundles.clone()).await;
    assert_eq!(
        cla.sink
            .dispatch(None, None, &mut arrival.clone())
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );
    svc.sink
        .send(&mut raw.clone())
        .await
        .expect("the first raw send admits");
    for _ in 0..2 {
        tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            forwarded_rx.recv_async(),
        )
        .await
        .expect("Timeout waiting for a first-life forward")
        .expect("Channel closed");
    }
    bpa.shutdown().await;

    // Second life over the same storage.
    let held = stored_bundles(&bundles).await;
    let (bpa, svc, cla, forwarded_rx) = restart_setup(metadata, bundles.clone()).await;
    assert_eq!(
        cla.sink
            .dispatch(None, None, &mut arrival.clone())
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );
    let result = svc.sink.send(&mut raw.clone()).await;
    assert!(matches!(result, Err(service::Error::DuplicateBundle)));

    // The completed shutdown is the barrier proving the absence.
    bpa.shutdown().await;
    assert!(forwarded_rx.is_empty(), "neither duplicate forwards");
    assert_eq!(
        stored_bundles(&bundles).await,
        held,
        "neither duplicate's spooled data is kept"
    );
}

/// An origination declaring more than `max_bundle_size` is refused before a
/// byte spools: `raw::Sink::send` from the wire size its headers declare,
/// read in the header pass, and `Sink::send_streamed` from its declared
/// payload. Both payload sources park after their first segments, so a
/// refusal returning at all proves the payload was never drained, and each
/// refusal reports a declared size rather than a count of bytes taken.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_originate_refuses_before_the_payload() {
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
        .max_bundle_size(NonZeroU64::new(2048).unwrap())
        .build();
    bpa.start(false).await.unwrap();

    let svc = bpa
        .register_raw_service(Some(ServiceId::Ipn(7)), EchoService::factory())
        .await
        .unwrap();
    let (factory, _service_rx) = TestService::factory();
    let service = bpa
        .register_service(Some(ServiceId::Ipn(42)), factory)
        .await
        .unwrap();

    // Raw: the headers declare the whole ~50 KB bundle. The timeout only
    // bounds a regression.
    let (_, data) = Builder::new("ipn:0.1.7".parse().unwrap(), "ipn:0.2.1".parse().unwrap())
        .with_payload(Cow::Owned(vec![0x5A_u8; 50_000]))
        .build(CreationTimestamp::now())
        .expect("Failed to build bundle");
    let mut stream = ParkedReceiver::new(&data, 1000, 3);
    let result = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        svc.sink.send(&mut stream),
    )
    .await
    .expect("Timeout: the raw size refusal must not wait for the payload");
    match result {
        Err(service::Error::PayloadTooLarge { size, max }) => {
            assert_eq!(size, data.len() as u64, "the declared wire size");
            assert_eq!(max, 2048);
        }
        other => panic!("expected PayloadTooLarge, got {other:?}"),
    }

    // ADU: a 50 KB declared payload, streamed. The timeout only bounds a
    // regression.
    let payload = vec![0u8; 50_000];
    let mut stream = ParkedReceiver::new(&payload, 1000, 3);
    let result = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        service.sink.send_streamed(
            "ipn:0.2.1".parse().unwrap(),
            50_000,
            &mut stream,
            Duration::from_secs(60),
            None,
        ),
    )
    .await
    .expect("Timeout: the ADU size refusal must not wait for the payload");
    match result {
        Err(service::Error::PayloadTooLarge { size, max }) => {
            assert!(size >= 50_000, "a declared size, got {size}");
            assert_eq!(max, 2048);
        }
        other => panic!("expected PayloadTooLarge, got {other:?}"),
    }

    bpa.shutdown().await;
}

// An ADU whose payload fits `max_bundle_size` but whose headers carry the
// bundle over it is refused by the shared stage's declared-size bound before
// the payload is pulled: the source parks on its first pull. The declared
// size is the built bundle's, whose length depends on the width of the
// creation timestamp's sequence number, so it is checked against twins built
// at one creation time with a sequence number of each CBOR width.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adu_originate_refuses_headers_over_the_cap() {
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
        .max_bundle_size(NonZeroU64::new(2048).unwrap())
        .build();
    bpa.start(false).await.unwrap();

    let (factory, _service_rx) = TestService::factory();
    let service = bpa
        .register_service(Some(ServiceId::Ipn(42)), factory)
        .await
        .unwrap();
    let source = service.endpoint.clone();
    let destination: Eid = "ipn:0.2.1".parse().unwrap();
    let lifetime = Duration::from_secs(60);

    let payload = vec![0u8; 2040];
    let creation_time = DtnTime::now();
    let twins: Vec<u64> = [0, 24, 256, 65_536]
        .into_iter()
        .map(|sequence_number| {
            let (_, data) = Builder::new(source.clone(), destination.clone())
                .with_lifetime(lifetime)
                .with_payload(Cow::Borrowed(&payload))
                .build(CreationTimestamp::from_parts(
                    Some(creation_time),
                    sequence_number,
                ))
                .expect("Failed to build bundle");
            data.len() as u64
        })
        .collect();
    assert!(
        twins.iter().all(|&size| size > 2048),
        "the headers carry every twin over the cap: {twins:?}"
    );

    // The timeout only bounds a regression.
    let mut stream = ParkedReceiver::new(&payload, 1000, 0);
    let result = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        service
            .sink
            .send_streamed(destination, 2040, &mut stream, lifetime, None),
    )
    .await
    .expect("Timeout: the size refusal must not wait for the payload");
    match result {
        Err(service::Error::PayloadTooLarge { size, max }) => {
            assert!(
                twins.contains(&size),
                "expected one of {twins:?}, got {size}"
            );
            assert_eq!(max, 2048);
        }
        other => panic!("expected PayloadTooLarge, got {other:?}"),
    }

    bpa.shutdown().await;
}

/// A builder-configured CLA goes live at `start`, not at `build`: activation
/// waits for storage recovery, so the quiescent-store contract covers the
/// configuration path exactly as it covers dynamic registrations.
#[tokio::test]
async fn configured_endpoints_activate_at_start() {
    struct ProbeCla {
        started_tx: flume::Sender<()>,
    }

    #[async_trait]
    impl cla::Cla for ProbeCla {
        async fn register(&self) -> cla::Result<()> {
            let _ = self.started_tx.send(());
            Ok(())
        }

        async fn forward(
            &self,
            _lane: Option<u32>,
            _cla_addr: &cla::Address,
            _bundle_id: &Id,
            _total_len: u64,
            _stream: &mut dyn Receiver<Segment>,
        ) -> cla::Result<cla::ForwardOutcome> {
            Ok(cla::ForwardOutcome::Sent)
        }
    }

    let (started_tx, started_rx) = flume::bounded(1);
    let bpa = Bpa::builder()
        .cla(
            "probe",
            factory_fn(Request::default(), |_cx: Context| {
                Ok(Arc::new(ProbeCla { started_tx }))
            }),
        )
        .build();
    assert!(
        started_rx.is_empty(),
        "a configured CLA must not go live before start"
    );

    bpa.start(false).await.unwrap();
    assert!(
        started_rx.try_recv().is_ok(),
        "start activates the configured CLA"
    );

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// The streamed ADU sink: Sink::send_streamed
// ---------------------------------------------------------------------------

/// A payload streamed through `send_streamed` originates a bundle
/// wire-equivalent to the whole-buffer `send` of the same payload: the same
/// source, destination, flags and payload bytes, with the ids differing only
/// by creation timestamp. The streamed form parses canonically, since the
/// CRC computed as the payload flowed is the CRC a resident build writes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app_send_streamed_matches_whole_buffer_send() {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).build();
    bpa.start(false).await.unwrap();

    let (factory, _service_rx) = TestService::factory();
    let service = bpa
        .register_service(Some(ServiceId::Ipn(42)), factory)
        .await
        .unwrap();

    let (cla, forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();
    let peer_addr = cla::Address::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    let dest: Eid = "ipn:0.2.1".parse().unwrap();
    let payload = vec![0xB7_u8; 9000];

    // Streamed: several Next segments and a Final.
    let (tx, mut rx) = hardy_async::channel::bounded(8);
    for chunk in payload.chunks(2000) {
        tx.send(Segment::Next(Bytes::copy_from_slice(chunk)))
            .await
            .unwrap();
    }
    tx.send(Segment::Final(Bytes::new())).await.unwrap();
    drop(tx);
    let streamed_id = service
        .sink
        .send_streamed(
            dest.clone(),
            payload.len() as u64,
            &mut rx,
            Duration::from_secs(3600),
            None,
        )
        .await
        .expect("streamed send failed");

    // Whole-buffer twin.
    let buffered_id = service
        .sink
        .send(
            dest.clone(),
            Bytes::from(payload.clone()),
            Duration::from_secs(3600),
            None,
        )
        .await
        .expect("whole-buffer send failed");

    // Event-driven waits; the timeouts only bound a regression.
    let mut parsed = Vec::new();
    for _ in 0..2 {
        let forwarded = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            forwarded_rx.recv_async(),
        )
        .await
        .expect("Timeout waiting for forwarded bundle")
        .expect("Channel closed");
        parsed.push(parse(forwarded).expect("forwarded bundle parses"));
    }
    parsed.sort_by_key(|p| p.bundle.primary.id != streamed_id);
    let (streamed, buffered) = (&parsed[0], &parsed[1]);

    assert_eq!(streamed.bundle.primary.id, streamed_id);
    assert_eq!(buffered.bundle.primary.id, buffered_id);
    assert_eq!(streamed.bundle.primary.destination, dest);
    assert_eq!(streamed.bundle.primary.flags, buffered.bundle.primary.flags);
    assert_eq!(
        streamed.bundle.blocks.len(),
        buffered.bundle.blocks.len(),
        "same block structure"
    );
    // Past the primary, which carries each bundle's own id, the two are the
    // same bytes.
    let after_primary = |p: &Parsed| {
        let end = usize::try_from(p.bundle.blocks[&0].extent.end).unwrap();
        p.data.slice(end..)
    };
    assert_eq!(after_primary(streamed), after_primary(buffered));
    assert_eq!(
        streamed.bundle.blocks[&1]
            .payload(&streamed.data)
            .expect("streamed payload resident"),
        payload.as_slice()
    );

    bpa.shutdown().await;
}

/// A producer that goes away before its final segment cancels the streamed
/// ADU send: `StreamCancelled`, nothing originated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app_send_streamed_producer_drop_cancels() {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).build();
    bpa.start(false).await.unwrap();

    let (factory, _service_rx) = TestService::factory();
    let service = bpa
        .register_service(Some(ServiceId::Ipn(42)), factory)
        .await
        .unwrap();
    let (cla, forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();
    let peer_addr = cla::Address::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    let (tx, mut rx) = hardy_async::channel::bounded(2);
    tx.send(Segment::Next(Bytes::from(vec![0u8; 1000])))
        .await
        .unwrap();
    drop(tx); // no Final: the producer aborts

    let result = service
        .sink
        .send_streamed(
            "ipn:0.2.1".parse().unwrap(),
            5000,
            &mut rx,
            Duration::from_secs(3600),
            None,
        )
        .await;
    assert!(matches!(result, Err(service::Error::StreamCancelled)));

    // The completed shutdown is the barrier proving the absence.
    bpa.shutdown().await;
    assert!(
        forwarded_rx.is_empty(),
        "a cancelled streamed send must not originate a bundle"
    );
}

/// The declared-length contract is enforced with nothing persisted: an
/// over-delivering producer is rejected `PayloadTooLarge`, an
/// under-delivering one `PayloadUnderrun`, each carrying the declared
/// total.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app_send_streamed_enforces_the_declared_length() {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).build();
    bpa.start(false).await.unwrap();

    let (factory, _service_rx) = TestService::factory();
    let service = bpa
        .register_service(Some(ServiceId::Ipn(42)), factory)
        .await
        .unwrap();
    let (cla, forwarded_rx) = PipelineCla::factory();
    let cla = bpa.register_cla("test", cla).await.unwrap();
    let peer_addr = cla::Address::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();
    let dest: Eid = "ipn:0.2.1".parse().unwrap();

    // Overrun: 2000 bytes against a declaration of 1000.
    let (tx, mut rx) = hardy_async::channel::bounded(2);
    tx.send(Segment::Final(Bytes::from(vec![0u8; 2000])))
        .await
        .unwrap();
    drop(tx);
    let result = service
        .sink
        .send_streamed(dest.clone(), 1000, &mut rx, Duration::from_secs(3600), None)
        .await;
    assert!(matches!(
        result,
        Err(service::Error::PayloadTooLarge { max: 1000, .. })
    ));

    // Underrun: 1000 bytes against a declaration of 2000.
    let (tx, mut rx) = hardy_async::channel::bounded(2);
    tx.send(Segment::Final(Bytes::from(vec![0u8; 1000])))
        .await
        .unwrap();
    drop(tx);
    let result = service
        .sink
        .send_streamed(dest.clone(), 2000, &mut rx, Duration::from_secs(3600), None)
        .await;
    assert!(matches!(
        result,
        Err(service::Error::PayloadUnderrun {
            size: 1000,
            expected: 2000
        })
    ));

    // A declaration past `max_bundle_size` is refused with its declared
    // length before the payload is pulled, however large that declaration
    // is: `u64::MAX` would otherwise overflow the built bundle's extents.
    // The timeout only bounds a regression.
    let mut stream = ParkedReceiver::new(&[], 1, 0);
    let result = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        service
            .sink
            .send_streamed(dest, u64::MAX, &mut stream, Duration::from_secs(3600), None),
    )
    .await
    .expect("Timeout: the size refusal must not wait for the payload");
    assert!(matches!(
        result,
        Err(service::Error::PayloadTooLarge { size: u64::MAX, .. })
    ));

    // The completed shutdown is the barrier proving the absence.
    bpa.shutdown().await;
    assert!(forwarded_rx.is_empty(), "nothing was originated");
}

/// The provided `send_streamed` buffers and delegates to `send` with the
/// same error surface, so a third-party [`Sink`], the gRPC bridge included,
/// has the streamed API without implementing it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app_send_streamed_default_buffers_and_delegates() {
    struct RecordingSink(flume::Sender<Bytes>);

    #[async_trait]
    impl Sink for RecordingSink {
        async fn unregister(&self) {}

        async fn send(
            &self,
            _destination: Eid,
            data: Bytes,
            _lifetime: Duration,
            _options: Option<SendOptions>,
        ) -> service::Result<Id> {
            self.0.send(data).unwrap();
            Ok(Id::default())
        }
    }

    let (tx, sent_rx) = flume::bounded(1);
    let sink = RecordingSink(tx);

    // A clean stream is buffered whole and handed to `send`.
    let payload = vec![0x11_u8; 3000];
    let (seg_tx, mut rx) = hardy_async::channel::bounded(4);
    for chunk in payload.chunks(1024) {
        seg_tx
            .send(Segment::Next(Bytes::copy_from_slice(chunk)))
            .await
            .unwrap();
    }
    seg_tx.send(Segment::Final(Bytes::new())).await.unwrap();
    drop(seg_tx);
    Sink::send_streamed(
        &sink,
        "ipn:0.2.1".parse().unwrap(),
        payload.len() as u64,
        &mut rx,
        Duration::from_secs(60),
        None,
    )
    .await
    .expect("default send_streamed delegates");
    assert_eq!(sent_rx.recv().unwrap().as_ref(), payload.as_slice());

    // The declared-length contract holds before `send` is ever called.
    let (seg_tx, mut rx) = hardy_async::channel::bounded(2);
    seg_tx
        .send(Segment::Final(Bytes::from(vec![0u8; 100])))
        .await
        .unwrap();
    drop(seg_tx);
    let result = Sink::send_streamed(
        &sink,
        "ipn:0.2.1".parse().unwrap(),
        500,
        &mut rx,
        Duration::from_secs(60),
        None,
    )
    .await;
    assert!(matches!(
        result,
        Err(service::Error::PayloadUnderrun {
            size: 100,
            expected: 500
        })
    ));
    assert!(sent_rx.is_empty(), "a violating stream never reaches send");
}
