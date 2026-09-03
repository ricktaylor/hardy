//! BPA Pipeline Integration Tests
//!
//! These tests verify end-to-end bundle processing through the BPA,
//! covering the component test plan (PLAN-BPA-01) Suites A and B.

use core::{
    num::{NonZeroU32, NonZeroU64},
    time::Duration,
};
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
    block::Type,
    builder::Builder,
    bundle::{Flags, Id},
    creation_timestamp::CreationTimestamp,
    dtn_time::DtnTime,
    editor::{Chunk, Editor},
    eid::{
        Service, {Eid, IpnNodeId, NodeId},
    },
    hop_info::HopInfo,
    parse::{Parsed, parse},
    status_report::ReasonCode,
};
use hardy_cbor::{
    decode::skip_value,
    encode::{Raw, emit, emit_array},
};
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
    sink: hardy_async::sync::spin::Once<Box<dyn cla::Sink>>,
    forwarded_tx: flume::Sender<Bytes>,
}

impl PipelineCla {
    fn new() -> (Arc<Self>, flume::Receiver<Bytes>) {
        let (tx, rx) = flume::bounded(16);
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                forwarded_tx: tx,
            }),
            rx,
        )
    }
}

#[async_trait]
impl cla::Cla for PipelineCla {
    fn lane_count(&self) -> Option<NonZeroU32> {
        None
    }

    async fn on_register(
        &self,
        sink: Box<dyn cla::Sink>,
        _node_ids: &[NodeId],
        _max_bundle_size: core::num::NonZeroU64,
    ) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn forward(
        &self,
        _queue: Option<u32>,
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

// ---------------------------------------------------------------------------
// Test Application — receives delivered bundles via a channel
// ---------------------------------------------------------------------------

struct TestApp {
    sink: hardy_async::sync::spin::Once<Box<dyn services::ApplicationSink>>,
    received_tx: flume::Sender<(Eid, Bytes)>,
}

impl TestApp {
    fn new() -> (Arc<Self>, flume::Receiver<(Eid, Bytes)>) {
        let (tx, rx) = flume::bounded(16);
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                received_tx: tx,
            }),
            rx,
        )
    }
}

#[async_trait]
impl services::Application for TestApp {
    async fn on_register(&self, _source: &Eid, sink: Box<dyn services::ApplicationSink>) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn on_deliver(
        &self,
        bundle_id: &Id,
        _expiry: time::OffsetDateTime,
        _ack_requested: bool,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        let payload = buffer_stream(stream, total_len).await?;
        self.received_tx
            .send((bundle_id.source.clone(), payload))
            .map_err(|e| services::Error::Internal(e.into()))
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
// Inline Echo Service — swaps source/destination and sends back
// ---------------------------------------------------------------------------

struct EchoService {
    sink: hardy_async::sync::spin::Once<Box<dyn services::ServiceSink>>,
}

impl EchoService {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            sink: hardy_async::sync::spin::Once::new(),
        })
    }
}

#[async_trait]
impl services::Service for EchoService {
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
        let data = buffer_stream(stream, total_len).await?;
        let Some(sink) = self.sink.get() else {
            return Ok(());
        };

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
        sink.send(&mut reply).await.map(|_| ())
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
// Timed CLA — captures arrival time inside forward() for accurate benchmarking
// ---------------------------------------------------------------------------

struct TimedCla {
    sink: hardy_async::sync::spin::Once<Box<dyn cla::Sink>>,
    arrival_tx: flume::Sender<tokio::time::Instant>,
}

impl TimedCla {
    fn new() -> (Arc<Self>, flume::Receiver<tokio::time::Instant>) {
        let (tx, rx) = flume::bounded(4096);
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                arrival_tx: tx,
            }),
            rx,
        )
    }
}

#[async_trait]
impl cla::Cla for TimedCla {
    fn lane_count(&self) -> Option<NonZeroU32> {
        None
    }

    async fn on_register(
        &self,
        sink: Box<dyn cla::Sink>,
        _node_ids: &[NodeId],
        _max_bundle_size: core::num::NonZeroU64,
    ) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn forward(
        &self,
        _queue: Option<u32>,
        _cla_addr: &cla::ClaAddress,
        _bundle_id: &Id,
        _total_len: u64,
        _stream: &mut dyn Receiver<cla::Segment>,
    ) -> cla::Result<cla::ForwardBundleResult> {
        let _ = self.arrival_tx.send(tokio::time::Instant::now());
        Ok(cla::ForwardBundleResult::Sent)
    }
}

// ---------------------------------------------------------------------------
// Helper: print system info for benchmark context
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
// Helper: build a bundle as raw bytes
// ---------------------------------------------------------------------------

fn build_bundle(source: &Eid, destination: &Eid, payload: &[u8]) -> Bytes {
    let (_, data) = Builder::new(source.clone(), destination.clone())
        .with_payload(Cow::Borrowed(payload))
        .build(CreationTimestamp::now())
        .expect("Failed to build bundle");
    Bytes::from(data)
}

// ---------------------------------------------------------------------------
// INT-BPA-01: App-to-CLA Routing
// ---------------------------------------------------------------------------

/// A locally-originated bundle addressed to a remote node is forwarded
/// to the correct CLA peer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app_to_cla_routing() {
    let bpa = Bpa::builder().build().await.unwrap();
    bpa.start(false).await;

    // Register CLA and add a peer for the remote node (ipn:0.2)
    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla("test".to_string(), cla.clone(), None, None)
        .await
        .unwrap();

    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(peer_addr, &[remote_node])
        .await
        .unwrap();

    // Register an application to send from
    let (app, _app_rx) = TestApp::new();
    let source_eid = bpa
        .register_application(Service::Ipn(42), app.clone())
        .await
        .unwrap();

    // Send a bundle to the remote node
    let dest: Eid = "ipn:0.2.99".parse().unwrap();
    app.sink
        .get()
        .unwrap()
        .send(
            dest.clone(),
            Bytes::from_static(b"Hello remote"),
            Duration::from_secs(3600),
            None,
        )
        .await
        .unwrap();

    // The CLA should forward the bundle
    // Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for forwarded bundle")
    .expect("Channel closed");

    // Parse and verify the forwarded bundle (structural — no keys).
    let Parsed {
        bundle: parsed_bundle,
        ..
    } = parse(forwarded).expect("Failed to parse forwarded bundle");

    assert_eq!(parsed_bundle.primary.id.source, source_eid);
    assert_eq!(parsed_bundle.primary.destination, dest);

    bpa.shutdown().await;
}

// A block the ingress gate schedules for §E removal (here an unrecognised
// extension block flagged `delete_block_on_failure`) is kept in the stored
// bundle — no editing on input — and stripped per attempt at the egress
// rewrite head, so the transmitted wire form no longer carries it while the
// payload survives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_removal_applied_at_egress() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla("test".to_string(), cla.clone(), None, None)
        .await
        .unwrap();
    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(peer_addr, &[remote])
        .await
        .unwrap();

    // Craft a bundle to the remote node carrying an unrecognised block (type
    // 999, block 2) flagged delete_block_on_failure, inserted between the
    // primary and the payload.
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

    // The crafted bundle really carries the unrecognised block as it arrives.
    let Parsed { bundle: pre, .. } = parse(inbound.clone()).expect("crafted bundle parses");
    assert!(
        pre.blocks
            .values()
            .any(|b| b.block_type == Type::Unrecognised(999)),
        "the unknown block is present as ingressed"
    );

    assert_eq!(
        cla.sink
            .get()
            .unwrap()
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
        "the deferred removal is applied at the egress door"
    );
    assert!(fwd.blocks.contains_key(&1), "the payload survives");

    bpa.shutdown().await;
}

// A low-level (raw-bundle) service that captures each delivered bundle's
// wire bytes.
struct CapturingService {
    sink: hardy_async::sync::spin::Once<Box<dyn services::ServiceSink>>,
    delivered_tx: flume::Sender<Bytes>,
}

impl CapturingService {
    fn new() -> (Arc<Self>, flume::Receiver<Bytes>) {
        let (delivered_tx, rx) = flume::unbounded();
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                delivered_tx,
            }),
            rx,
        )
    }
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
        let data = buffer_stream(stream, total_len).await?;
        let _ = self.delivered_tx.send(data);
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

// The deliver-side twin of `deferred_removal_applied_at_egress`: a bundle
// addressed to a local raw-bundle service, carrying an unrecognised
// `delete_block_on_failure` block, is delivered with that block stripped —
// the stored bundle stays as received.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_removal_applied_at_delivery() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    let (svc, delivered_rx) = CapturingService::new();
    let endpoint = bpa.register_service(Service::Ipn(7), svc).await.unwrap();

    let (cla, _forwarded_rx) = PipelineCla::new();
    bpa.register_cla("test".to_string(), cla.clone(), None, None)
        .await
        .unwrap();

    // A bundle to the local service, carrying an unrecognised block (type
    // 999, block 2) flagged delete_block_on_failure.
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
            .get()
            .unwrap()
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
        "the deferred removal is applied at the deliver door"
    );
    assert!(del.blocks.contains_key(&1), "the payload survives");

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

    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    // Register echo service on service number 7
    let echo = EchoService::new();
    bpa.register_service(Service::Ipn(7), echo).await.unwrap();

    // Register CLA with a peer for the "remote" node (ipn:0.2)
    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla("test".to_string(), cla.clone(), None, None)
        .await
        .unwrap();

    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    // Build an inbound bundle: from remote node, to our echo service
    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let echo_dest: Eid = "ipn:0.1.7".parse().unwrap();
    let mut inbound = build_bundle(&remote_source, &echo_dest, b"ping");

    // Dispatch it as if received from the CLA
    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(Some(&remote_node), None, &mut inbound)
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // The echo service should reflect the bundle back:
    // source=ipn:0.1.7 (echo), dest=ipn:0.2.1 (remote)
    // BPA routes to CLA peer (ipn:0.2.*)
    // Event-driven wait; the timeout only bounds a regression.
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for echo reply")
    .expect("Channel closed");

    // Parse and verify the echo reply (structural — no keys).
    let Parsed {
        bundle: parsed_bundle,
        ..
    } = parse(forwarded).expect("Failed to parse echo reply");

    // Source and destination should be swapped
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

// Register a service and a CLA peer; the returned (bpa, sink-holder, forwarded
// channel, service EID) drive ServiceSink::send directly with multi-segment streams.
async fn streamed_originate_setup() -> (Bpa, Arc<EchoService>, flume::Receiver<Bytes>, Eid) {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();

    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    // The service only captures its sink here; the test drives the sink itself.
    let svc = EchoService::new();
    bpa.register_service(Service::Ipn(7), svc.clone())
        .await
        .unwrap();

    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla("test".to_string(), cla.clone(), None, None)
        .await
        .unwrap();

    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    (bpa, svc, forwarded_rx, "ipn:0.1.7".parse().unwrap())
}

/// A bundle streamed through `ServiceSink::send` in several segments
/// originates identically to a whole-buffer `send`: the returned id matches
/// the built bundle, and the bundle is forwarded to the CLA peer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_streamed_originate_forwards() {
    let (bpa, svc, forwarded_rx, source_eid) = streamed_originate_setup().await;

    let dest: Eid = "ipn:0.2.1".parse().unwrap();
    let data = build_bundle(&source_eid, &dest, b"streamed-originate");

    // Split into two Next segments and a Final; the channel is sized to hold
    // them all so the producer side completes before the sink pulls.
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

    let id = svc
        .sink
        .get()
        .unwrap()
        .send(&mut rx)
        .await
        .expect("streamed send failed");
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

/// Dropping the producer before `Final` cancels the send: the caller gets
/// `StreamCancelled` and nothing enters custody (nothing is forwarded).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_streamed_cancel_stores_nothing() {
    let (bpa, svc, forwarded_rx, source_eid) = streamed_originate_setup().await;

    let dest: Eid = "ipn:0.2.1".parse().unwrap();
    let data = build_bundle(&source_eid, &dest, b"cancelled");

    let (tx, mut rx) = hardy_async::channel::bounded(4);
    tx.send(Segment::Next(data.slice(..data.len() / 2)))
        .await
        .unwrap();
    drop(tx); // no Final — the producer aborts

    let result = svc.sink.get().unwrap().send(&mut rx).await;
    assert!(matches!(
        result,
        Err(hardy_bpa::services::Error::StreamCancelled)
    ));

    // Nothing was originated: no forward may arrive.
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

/// Unregistering a service wakes its in-flight sends immediately: a consumer
/// parked behind a stalled producer fails with `StreamCancelled` the moment
/// the registration dies, without waiting for the producer's next segment.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_unregister_cancels_parked_send() {
    let (bpa, svc, _forwarded_rx, source_eid) = streamed_originate_setup().await;

    let dest: Eid = "ipn:0.2.1".parse().unwrap();
    let data = build_bundle(&source_eid, &dest, b"parked");

    // One segment then a stall — the sender stays alive throughout, so only
    // registration teardown can end the stream.
    let (tx, mut rx) = hardy_async::channel::bounded(2);
    hardy_async::channel::Sender::send(&tx, Segment::Next(data.slice(..data.len() / 2)))
        .await
        .unwrap();

    let parked = {
        let svc = svc.clone();
        tokio::spawn(async move { svc.sink.get().unwrap().send(&mut rx).await })
    };

    // Let the consumer enter the stream and park on the second pull.
    // Known test-guide deviation (timed quiesce): scheduled for the
    // dedicated pipeline de-flake pass (see bpa/docs/TODO.md).
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    svc.sink.get().unwrap().unregister().await;

    let result = tokio::time::timeout(tokio::time::Duration::from_secs(5), parked)
        .await
        .expect("parked send was not woken by unregistration")
        .expect("task panicked");
    assert!(matches!(
        result,
        Err(hardy_bpa::services::Error::StreamCancelled)
    ));
    drop(tx);

    bpa.shutdown().await;
}

/// The source-spoof check holds on the streamed path: a bundle whose source
/// is not the registered service endpoint is rejected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_streamed_send_rejects_spoofed_source() {
    let (bpa, svc, _forwarded_rx, _source_eid) = streamed_originate_setup().await;

    let spoofed: Eid = "ipn:0.1.99".parse().unwrap();
    let dest: Eid = "ipn:0.2.1".parse().unwrap();
    let data = build_bundle(&spoofed, &dest, b"spoofed");

    let (tx, mut rx) = hardy_async::channel::bounded(1);
    tx.send(Segment::Final(data)).await.unwrap();
    drop(tx);

    let result = svc.sink.get().unwrap().send(&mut rx).await;
    assert!(matches!(
        result,
        Err(hardy_bpa::services::Error::InvalidDestination(_))
    ));

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// INT-BPA-03: Local Delivery
// ---------------------------------------------------------------------------

/// A bundle dispatched via CLA addressed to a local application is
/// delivered to that application's on_deliver callback.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_delivery() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();

    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    // Register an application on service number 42
    let (app, app_rx) = TestApp::new();
    bpa.register_application(Service::Ipn(42), app.clone())
        .await
        .unwrap();

    // Register CLA (needed for dispatch)
    let (cla, _forwarded_rx) = PipelineCla::new();
    bpa.register_cla("test".to_string(), cla.clone(), None, None)
        .await
        .unwrap();

    // Build an inbound bundle addressed to our local application
    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let local_dest: Eid = "ipn:0.1.42".parse().unwrap();
    let mut inbound = build_bundle(&remote_source, &local_dest, b"Hello local");

    // Dispatch via CLA
    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(None, None, &mut inbound)
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // Application should receive the payload
    let (source, payload) =
        // Event-driven wait; the timeout only bounds a regression.
        tokio::time::timeout(tokio::time::Duration::from_secs(5), app_rx.recv_async())
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
// Reception report reason codes (RFC 9172 §7.1)
// ---------------------------------------------------------------------------

// Splice a BCB with an unrecognised security context (id 99) targeting the
// payload into `data` as block number 2, flagged must-replicate (required
// for a payload target) + report-on-failure.
fn splice_unrecognised_bcb(data: &[u8]) -> Bytes {
    // ASB CBOR sequence: targets [1], context id 99, context flags 0 (no
    // parameters), source EID, then one result list per target.
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

// A transit bundle carrying a BCB this node cannot understand (unrecognised
// security context, `report_on_failure` set) is still forwarded, and the
// requested reception report carries the RFC 9172 `UnknownSecurityOperation`
// reason rather than the generic `BlockUnsupported`. The bundle is addressed
// to a remote node so it forwards — a payload-targeting BCB is only ever
// decrypted at delivery.
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
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    // Register CLA with a peer for the remote node (ipn:0.2) — the route for
    // both the forwarded bundle and the reception report (report-to defaults
    // to the source).
    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla("test".to_string(), cla.clone(), None, None)
        .await
        .unwrap();
    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
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
            .get()
            .unwrap()
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

    // The original is forwarded intact, BCB and all (an operation we cannot
    // understand is left for a downstream security acceptor).
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
/// ingresses identically to the whole-buffer `dispatch`: the reassembled
/// bundle reaches its local application.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cla_streamed_ingress_delivers() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();

    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    let (app, app_rx) = TestApp::new();
    bpa.register_application(Service::Ipn(42), app.clone())
        .await
        .unwrap();

    let (cla, _forwarded_rx) = PipelineCla::new();
    bpa.register_cla("test".to_string(), cla.clone(), None, None)
        .await
        .unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let local_dest: Eid = "ipn:0.1.42".parse().unwrap();
    let inbound = build_bundle(&remote_source, &local_dest, b"streamed ingress");

    // Three segments through a bounded channel with a spawned producer, so
    // the pull side is genuinely driving.
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
        cla.sink
            .get()
            .unwrap()
            .dispatch(None, None, &mut rx)
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );
    producer.await.unwrap();

    let (source, payload) =
        // Event-driven wait; the timeout only bounds a regression.
        tokio::time::timeout(tokio::time::Duration::from_secs(5), app_rx.recv_async())
            .await
            .expect("Timeout waiting for streamed delivery")
            .expect("Channel closed");
    assert_eq!(source, remote_source);
    assert_eq!(payload.as_ref(), b"streamed ingress");

    bpa.shutdown().await;
}

/// Unregistering a CLA wakes its in-flight streams immediately: a consumer
/// parked behind a stalled producer fails with `StreamCancelled` the moment
/// the registration dies, without waiting for the producer's next segment.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cla_unregister_cancels_parked_stream() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();

    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    let (cla, _forwarded_rx) = PipelineCla::new();
    bpa.register_cla("test".to_string(), cla.clone(), None, None)
        .await
        .unwrap();

    // A producer that sends one segment then stalls — the sender stays
    // alive throughout, so only registration teardown can end the stream.
    // The segment is a genuine bundle prefix: the ingress gate parses
    // eagerly, so arbitrary bytes would be rejected structurally instead
    // of leaving the consumer parked on the next pull.
    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let local_dest: Eid = "ipn:0.1.42".parse().unwrap();
    let inbound = build_bundle(&remote_source, &local_dest, b"parked");
    let (tx, mut rx) = hardy_async::channel::bounded(2);
    hardy_async::channel::Sender::send(&tx, Segment::Next(inbound.slice(..inbound.len() / 2)))
        .await
        .unwrap();

    let parked = {
        let cla = cla.clone();
        tokio::spawn(async move { cla.sink.get().unwrap().dispatch(None, None, &mut rx).await })
    };

    // Let the consumer enter the stream and park on the second pull.
    // Known test-guide deviation (timed quiesce): scheduled for the
    // dedicated pipeline de-flake pass (see bpa/docs/TODO.md).
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    cla.sink.get().unwrap().unregister().await;

    // The parked pull must fail promptly — the assertion is event-driven;
    // the timeout only bounds a regression. The teardown is reported as the
    // dead registration, not a per-bundle refusal.
    let result = tokio::time::timeout(tokio::time::Duration::from_secs(5), parked)
        .await
        .expect("parked stream was not woken by unregistration")
        .expect("task panicked");
    assert!(matches!(result, Err(hardy_bpa::cla::Error::Disconnected)));
    drop(tx);

    bpa.shutdown().await;
}

/// A producer that dies before `Final` is a truncation: the sink refuses
/// acceptance (so a CLA withholds its transfer ack) and nothing is
/// delivered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cla_streamed_ingress_truncation_is_refused() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();

    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    let (app, app_rx) = TestApp::new();
    bpa.register_application(Service::Ipn(42), app.clone())
        .await
        .unwrap();

    let (cla, _forwarded_rx) = PipelineCla::new();
    bpa.register_cla("test".to_string(), cla.clone(), None, None)
        .await
        .unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let local_dest: Eid = "ipn:0.1.42".parse().unwrap();
    let inbound = build_bundle(&remote_source, &local_dest, b"truncated");

    let (tx, mut rx) = hardy_async::channel::bounded(4);
    hardy_async::channel::Sender::send(&tx, Segment::Next(inbound.slice(..inbound.len() / 2)))
        .await
        .unwrap();
    drop(tx); // no Final

    let result = cla.sink.get().unwrap().dispatch(None, None, &mut rx).await;
    assert!(matches!(result, Ok(cla::Acceptance::Refused)));

    // Known test-guide deviation (quiet-window absence assert):
    // scheduled for the dedicated pipeline de-flake pass (see bpa/docs/TODO.md).
    assert!(
        tokio::time::timeout(tokio::time::Duration::from_millis(500), app_rx.recv_async())
            .await
            .is_err(),
        "truncated stream must not deliver"
    );

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// Streamed oversized-payload ingress: a CLA delivers a bundle whose payload
// exceeds the parser chunk size, split across many segments — exercising the
// `Partial` / `drain_tail` (dumb-spool) path end-to-end.
// ---------------------------------------------------------------------------

// A `Receiver` that yields a fixed sequence of segments then reports the
// producer is gone — mimics a CLA reassembling a transfer into segments.
struct SegmentReceiver {
    segments: Mutex<VecDeque<cla::Segment>>,
}

impl SegmentReceiver {
    // Split `data` into `chunk`-byte segments: `Next` for all but the last,
    // which is `Final`.
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

    // Segments not yet pulled by the parser. Non-zero after dispatch means the
    // payload tail was never drained.
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

// An inbound bundle with a payload far larger than the 4096-byte parser chunk
// size, delivered in 1000-byte segments, is reassembled and delivered intact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streamed_oversized_payload_local_delivery() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    let (app, app_rx) = TestApp::new();
    bpa.register_application(Service::Ipn(42), app.clone())
        .await
        .unwrap();

    let (cla, _forwarded_rx) = PipelineCla::new();
    bpa.register_cla("test".to_string(), cla.clone(), None, None)
        .await
        .unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let local_dest: Eid = "ipn:0.1.42".parse().unwrap();
    let payload = vec![0xA5_u8; 20_000];
    let inbound = build_bundle(&remote_source, &local_dest, &payload);
    assert!(
        inbound.len() > 4096,
        "payload must exceed the parser chunk size"
    );

    // Deliver the bundle as a stream of 1000-byte segments (forces Partial).
    let mut stream = SegmentReceiver::new(&inbound, 1000);
    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(None, None, &mut stream)
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    let (source, delivered) =
        // Event-driven wait; the timeout only bounds a regression.
        tokio::time::timeout(tokio::time::Duration::from_secs(5), app_rx.recv_async())
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

// A bundle whose creation time + lifetime is already in the past — expired on
// arrival. Oversized payload so it streams as `Partial`.
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
// Oversized payload so it streams as `Partial`.
fn build_hop_exhausted_bundle(source: &Eid, destination: &Eid, payload: &[u8]) -> Bytes {
    let hop = HopInfo { limit: 1, count: 2 };
    let (_, data) = Builder::new(source.clone(), destination.clone())
        .with_hop_count(&hop)
        .with_payload(Cow::Borrowed(payload))
        .build(CreationTimestamp::now())
        .expect("Failed to build hop-exhausted bundle");
    Bytes::from(data)
}

// The §5.4 early-reject gate: a streamed bundle that fails a header-only check
// (lifetime, hop count) is dropped *before* its payload tail is drained, so the
// CLA never has to spool a gigantic invalid payload. Asserted by counting the
// segments left un-pulled in the `SegmentReceiver`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streamed_oversized_gate_drops_before_draining_payload() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    let (app, app_rx) = TestApp::new();
    bpa.register_application(Service::Ipn(42), app.clone())
        .await
        .unwrap();

    let (cla, _forwarded_rx) = PipelineCla::new();
    bpa.register_cla("test".to_string(), cla.clone(), None, None)
        .await
        .unwrap();
    let sink = || cla.sink.get().unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let local_dest: Eid = "ipn:0.1.42".parse().unwrap();
    let payload = vec![0xA5_u8; 20_000];

    // Expired — gated on lifetime; the payload tail must stay un-pulled.
    let expired = build_expired_bundle(&remote_source, &local_dest, &payload);
    assert!(expired.len() > 4096, "payload must exceed the parser chunk");
    let mut stream = SegmentReceiver::new(&expired, 1000);
    assert_eq!(
        sink().dispatch(None, None, &mut stream).await.unwrap(),
        cla::Acceptance::Accepted
    );
    assert!(
        stream.remaining() > 0,
        "expired bundle must be dropped before the payload tail is drained"
    );

    // Hop-exhausted — gated on hop count; same expectation.
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

    // Control: a valid bundle of the same size passes the gate, drains fully,
    // and delivers — proving the un-pulled segments above are the gate at work,
    // not a stalled stream.
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
        tokio::time::timeout(tokio::time::Duration::from_secs(5), app_rx.recv_async())
            .await
            .expect("Timeout waiting for the valid control bundle")
            .expect("Channel closed");
    assert_eq!(delivered.as_ref(), payload.as_slice());
    assert!(
        app_rx.is_empty(),
        "neither gated bundle should have been delivered"
    );

    bpa.shutdown().await;
}

// The gate's reporting split: a hop-exhausted arrival with the report flags
// set emits the combined §5.6/§5.10 status report — one §6.1.1 record
// asserting both reception and deletion, citing `HopLimitExceeded` — while
// an already-expired arrival with the same flags emits nothing at all
// (anti-amplification: it is treated as if it never arrived). Swapping the
// two gate branches fails both halves.
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
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    // A CLA with a peer for the remote node — the route for the reports
    // (report-to defaults to the source).
    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla("test".to_string(), cla.clone(), None, None)
        .await
        .unwrap();
    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
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
        .with_hop_count(&HopInfo { limit: 1, count: 2 })
        .with_payload(Cow::Borrowed(b"opaque".as_slice()))
        .build(CreationTimestamp::now())
        .unwrap();
    let mut inbound = Bytes::from(data);
    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(Some(&remote_node), None, &mut inbound)
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // The single combined status report comes out of the CLA — reception and
    // deletion asserted in one record; the bundle itself must not be
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

    // An already-expired arrival with the same report flags: total silence.
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
            .get()
            .unwrap()
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

// A complete-but-invalid streamed payload — a CRC mismatch the drain's
// `TailReceiver` detects after the header pass admitted the bundle — is
// accepted, terminated, and reported exactly like a gate drop: the §5.6/§5.10
// reception + deletion pair, the deletion citing `BlockUnintelligible`. The
// transfer is accepted, never refused: the content cannot become valid by
// resending.
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
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    // A CLA with a peer for the remote node — the route for the reports
    // (report-to defaults to the source).
    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla("test".to_string(), cla.clone(), None, None)
        .await
        .unwrap();
    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let dest: Eid = "ipn:0.2.99".parse().unwrap();

    // An oversized-payload bundle fed in CLA-sized chunks: the payload
    // outruns the parser's accumulation, so the bundle takes the `Partial`
    // route and the payload streams through the validating drain. One
    // corrupt byte deep in the payload body fails the payload CRC there —
    // after the header pass admitted the bundle.
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
    let corrupt_at = data.len() - 100; // inside the payload body, before its CRC field
    data[corrupt_at] ^= 0xFF;

    let mut stream = SegmentReceiver::new(&data, 1000);
    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(Some(&remote_node), None, &mut stream)
            .await
            .unwrap(),
        cla::Acceptance::Accepted,
        "a complete-but-corrupt transfer is accepted and terminated, never refused"
    );

    // The single combined status report comes out of the CLA — reception and
    // deletion asserted in one record; the bundle itself must not be
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
    // node — one report bundle, and never the rejected bundle itself.
    bpa.shutdown().await;
    assert!(
        forwarded_rx.is_empty(),
        "exactly one report leaves the node"
    );
}

// A keyed header-pass failure with a recoverable bundle id — here an
// unrecognised extension block flagged `delete_bundle_on_failure`, fatal at
// the §A classify — is reported exactly like a gate or drain drop: one
// status report asserting both reception and deletion, citing
// `BlockUnsupported` (§5.6 Step 4's delete-bundle case). The transfer is
// accepted: the content cannot become valid by resending.
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
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    // A CLA with a peer for the remote node — the route for the reports
    // (report-to defaults to the source).
    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla("test".to_string(), cla.clone(), None, None)
        .await
        .unwrap();
    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let dest: Eid = "ipn:0.2.99".parse().unwrap();

    // A report-requesting bundle carrying an unrecognised block (type 999,
    // block 2) flagged delete_bundle_on_failure, spliced between the primary
    // and the payload.
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
            .get()
            .unwrap()
            .dispatch(Some(&remote_node), None, &mut inbound)
            .await
            .unwrap(),
        cla::Acceptance::Accepted,
        "a complete-but-unsupported bundle is accepted and terminated, never refused"
    );

    // The single combined status report comes out of the CLA — reception and
    // deletion asserted in one record; the bundle itself must not be
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
    // node — one report bundle, and never the rejected bundle itself.
    bpa.shutdown().await;
    assert!(
        forwarded_rx.is_empty(),
        "exactly one report leaves the node"
    );
}

// A rejected bundle that requested only reception reporting (no deletion
// reports) still carries the §5.6 Step-4 facts on its reception-only status
// report: an unrecognised `report_on_failure` block sets the reception
// reason to `BlockUnsupported`, and a hop-exhausted gate drop must not
// squash it to `NoAdditionalInformation` — with no deletion asserted, the
// record's one reason slot belongs to the reception assertion.
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
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla("test".to_string(), cla.clone(), None, None)
        .await
        .unwrap();
    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let dest: Eid = "ipn:0.2.99".parse().unwrap();

    // Hop-exhausted, reception-report-only bundle carrying an unrecognised
    // block (type 999, block 3 — the builder's Hop Count block takes 2)
    // flagged report_on_failure.
    let (_, data) = Builder::new(remote_source.clone(), dest)
        .with_flags(Flags {
            receipt_report_requested: true,
            ..Default::default()
        })
        .with_hop_count(&HopInfo { limit: 1, count: 2 })
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
            .get()
            .unwrap()
            .dispatch(Some(&remote_node), None, &mut inbound)
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // One reception-only report: received asserted with the Step-4 reason,
    // no deletion asserted (deletion reports were not requested).
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
    // node — one report bundle, and never the rejected bundle itself.
    bpa.shutdown().await;
    assert!(
        forwarded_rx.is_empty(),
        "exactly one report leaves the node"
    );
}

// §5.6 Step 4's block-flag-alone trigger: a bundle requesting NO status
// reports at bundle level, carrying an unrecognised block flagged
// `report_on_failure`, still generates a reception report citing
// `BlockUnsupported` — the block's own flag is the request. The bundle
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
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla("test".to_string(), cla.clone(), None, None)
        .await
        .unwrap();
    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let dest: Eid = "ipn:0.2.99".parse().unwrap();

    // No bundle-level report flags at all; unrecognised block (type 999,
    // block 2) flagged report_on_failure, spliced after the primary.
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
            .get()
            .unwrap()
            .dispatch(Some(&remote_node), None, &mut inbound)
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    // Two bundles leave the node in either order: the forced reception
    // report (to the source) and the forwarded original.
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

    // The original forwards intact — a report flag never removes the block.
    let original = original.expect("original bundle forwarded");
    assert!(
        original
            .bundle
            .blocks
            .values()
            .any(|b| b.block_type == Type::Unrecognised(999)),
        "the unrecognised block rides on unchanged"
    );

    // The report asserts reception only, citing the Step-4 reason.
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

// The null-endpoint carve-out for the forced report: the same
// block-demanded bundle whose report-to is `dtn:none` produces no report —
// there is nowhere to send it. (A bundle with no bundle-level report flags
// may lawfully carry a null report-to; only the §4.2.3 flag combinations
// are parse-rejected.) The bundle still forwards.
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
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla("test".to_string(), cla.clone(), None, None)
        .await
        .unwrap();
    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
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
            .get()
            .unwrap()
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

// R-01: a single `Segment::Final` carrying a bundle whose declared payload is
// truncated — the parser takes the streaming fallback (`Partial`) though the
// stream has already ended — must be an internal drop, not handed to the
// payload drain (which would await an exhausted stream: a hang, or a spurious
// `StreamCancelled` driving unbounded peer retransmit of a permanently-invalid
// bundle).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streamed_truncated_final_segment_is_dropped_not_cancelled() {
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
    let (app, app_rx) = TestApp::new();
    bpa.register_application(Service::Ipn(42), app.clone())
        .await
        .unwrap();
    let (cla, _fwd) = PipelineCla::new();
    bpa.register_cla("test".to_string(), cla.clone(), None, None)
        .await
        .unwrap();

    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let local_dest: Eid = "ipn:0.1.42".parse().unwrap();
    // A 20 KB-payload bundle truncated to 8 KB: the payload byte-string header
    // still claims 20 KB (shortfall exceeds the 4096 parser chunk, forcing
    // `Partial`), but the body is short and the stream ends in this one `Final`.
    let full = build_bundle(&remote_source, &local_dest, &vec![0xA5_u8; 20_000]);
    let truncated = Bytes::copy_from_slice(&full[..8_000]);
    let mut stream = SegmentReceiver::new(&truncated, truncated.len()); // one Final

    // The timeout only bounds a regression: before the fix this parked forever
    // on the exhausted stream (or returned StreamCancelled).
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        cla.sink.get().unwrap().dispatch(None, None, &mut stream),
    )
    .await
    .expect("a truncated Final must not hang the ingress task");
    assert!(
        result.is_ok(),
        "a truncated complete transfer is an internal drop, not StreamCancelled: {result:?}"
    );

    bpa.shutdown().await;
    assert!(
        app_rx.is_empty(),
        "the truncated bundle must not be delivered"
    );
}

// R-04: the ingress size cap refuses an over-cap bundle at both
// enforcement points — header accumulation (`HeaderFailure::TooLarge`) and the
// payload drain (`DrainFailure::TooLarge`) — surfacing `Acceptance::Refused`
// so the CLA withholds the transfer ack.
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

    // Header phase: a cap below the first accumulated segment trips
    // `parse_headers` before the header chain even completes.
    {
        let bpa = Bpa::builder()
            .node_ids(node_ids.clone())
            .max_bundle_size(NonZeroU64::new(256).unwrap())
            .build()
            .await
            .unwrap();
        bpa.start(false).await;
        let (cla, _fwd) = PipelineCla::new();
        bpa.register_cla("test".to_string(), cla.clone(), None, None)
            .await
            .unwrap();
        let mut stream = SegmentReceiver::new(&inbound, 1000);
        assert_eq!(
            cla.sink
                .get()
                .unwrap()
                .dispatch(None, None, &mut stream)
                .await
                .unwrap(),
            cla::Acceptance::Refused,
            "header-phase over-cap must be refused"
        );
        bpa.shutdown().await;
    }

    // Drain phase: a cap that admits the header (so parse yields `Partial`) but
    // not the payload tail trips `drain_payload`.
    {
        let bpa = Bpa::builder()
            .node_ids(node_ids.clone())
            .max_bundle_size(NonZeroU64::new(10_000).unwrap())
            .build()
            .await
            .unwrap();
        bpa.start(false).await;
        let (cla, _fwd) = PipelineCla::new();
        bpa.register_cla("test".to_string(), cla.clone(), None, None)
            .await
            .unwrap();
        let mut stream = SegmentReceiver::new(&inbound, 1000);
        assert_eq!(
            cla.sink
                .get()
                .unwrap()
                .dispatch(None, None, &mut stream)
                .await
                .unwrap(),
            cla::Acceptance::Refused,
            "drain-phase over-cap must be refused"
        );
        bpa.shutdown().await;
    }
}

// ---------------------------------------------------------------------------
// PERF-01: Throughput (REQ-13: >1000 bundles/sec)
// ---------------------------------------------------------------------------

/// Measures bundle forwarding throughput through the BPA pipeline.
/// Dispatches bundles via CLA, routes to a peer, signals on the other side.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn throughput() {
    print_system_info();
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();

    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    let (cla, arrival_rx) = TimedCla::new();
    bpa.register_cla("test".to_string(), cla.clone(), None, None)
        .await
        .unwrap();

    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    let src: Eid = "ipn:0.3.1".parse().unwrap();
    let dst: Eid = "ipn:0.2.99".parse().unwrap();
    let count = 1000usize;

    // Pre-generate all bundles with unique IDs (avoids construction overhead in measurement)
    let warmup_bundles: Vec<_> = (0..10)
        .map(|_| build_bundle(&src, &dst, b"warmup"))
        .collect();
    let test_bundles: Vec<_> = (0..count)
        .map(|_| build_bundle(&src, &dst, b"throughput"))
        .collect();

    // Warm up
    for (i, mut bundle) in warmup_bundles.into_iter().enumerate() {
        assert_eq!(
            cla.sink
                .get()
                .unwrap()
                .dispatch(None, None, &mut bundle)
                .await
                .unwrap(),
            cla::Acceptance::Accepted
        );
        // Event-driven wait; the timeout only bounds a regression.
        tokio::time::timeout(tokio::time::Duration::from_secs(5), arrival_rx.recv_async())
            .await
            .unwrap_or_else(|_| panic!("Timeout waiting for warmup bundle {i}"))
            .unwrap();
    }

    // Measure — serial dispatch+receive to avoid backpressure and duplicate issues.
    // Each bundle is dispatched and received before the next is sent.
    let start = tokio::time::Instant::now();
    let mut last_arrival = start;
    for (i, mut bundle) in test_bundles.into_iter().enumerate() {
        assert_eq!(
            cla.sink
                .get()
                .unwrap()
                .dispatch(None, None, &mut bundle)
                .await
                .unwrap(),
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

    // REQ-13: >1000 bundles/sec (in-memory, no I/O). Coverage instrumentation
    // slows the pipeline below the target, so the gate is advisory there;
    // REQ-13 is formally verified by the criterion benchmark.
    if env::var_os("CARGO_LLVM_COV").is_none() {
        assert!(
            bundles_per_sec > 1000.0,
            "Throughput {bundles_per_sec:.0} bundles/sec below REQ-13 target of 1000"
        );
    }

    // Don't call bpa.shutdown() — 1000 ForwardPending bundles in metadata
    // cause the internal poller to re-poll indefinitely during shutdown.
    // The BPA is leaked; the runtime cleans up on test exit.
}

// ---------------------------------------------------------------------------
// PERF-LAT-01: Forwarding Latency
// ---------------------------------------------------------------------------

/// Measures per-bundle forwarding latency through the BPA pipeline.
/// Unidirectional: CLA dispatch → BPA route → CLA forward.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forwarding_latency() {
    print_system_info();
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();

    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    let (cla, arrival_rx) = TimedCla::new();
    bpa.register_cla("test".to_string(), cla.clone(), None, None)
        .await
        .unwrap();

    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(peer_addr, from_ref(&remote_node))
        .await
        .unwrap();

    let src: Eid = "ipn:0.3.1".parse().unwrap();
    let dst: Eid = "ipn:0.2.99".parse().unwrap();
    let count = 100usize;

    // Pre-generate all bundles with unique IDs
    let warmup_bundles: Vec<_> = (0..10)
        .map(|_| build_bundle(&src, &dst, b"warmup"))
        .collect();
    let test_bundles: Vec<_> = (0..count)
        .map(|_| build_bundle(&src, &dst, b"latency"))
        .collect();

    // Warm up
    for (i, mut bundle) in warmup_bundles.into_iter().enumerate() {
        assert_eq!(
            cla.sink
                .get()
                .unwrap()
                .dispatch(None, None, &mut bundle)
                .await
                .unwrap(),
            cla::Acceptance::Accepted
        );
        // Event-driven wait; the timeout only bounds a regression.
        tokio::time::timeout(tokio::time::Duration::from_secs(5), arrival_rx.recv_async())
            .await
            .unwrap_or_else(|_| panic!("Timeout waiting for warmup bundle {i}"))
            .unwrap();
    }

    // Measure individual dispatch-to-forward latencies.
    // The arrival time is sampled inside forward(), so we measure the
    // actual pipeline processing time, not the channel wait.
    let mut latencies = Vec::with_capacity(count);

    for (i, mut bundle) in test_bundles.into_iter().enumerate() {
        let dispatched = tokio::time::Instant::now();
        assert_eq!(
            cla.sink
                .get()
                .unwrap()
                .dispatch(None, None, &mut bundle)
                .await
                .unwrap(),
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

    // Drop receiver to unblock any poller send_async, then shutdown
    drop(arrival_rx);
    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// INT-BPA-06: Egress filter bundle/data consistency
// ---------------------------------------------------------------------------

/// Records any divergence between the Bundle's block extents and the wire
/// data it is handed alongside, observed through the reader's extent-based
/// block access: a stale block map over rewritten bytes reads back shifted
/// garbage.
struct ExtentCheckVerifier {
    mismatch: Arc<Mutex<Option<String>>>,
}

impl hardy_bpa::filter::Verifier for ExtentCheckVerifier {
    fn check(&self, reader: &hardy_bpa::filter::BundleReader<'_>) -> hardy_bpa::filter::Verdict {
        let mut mismatch = self.mismatch.lock().unwrap();

        // The payload extent must index the rewritten bytes.
        match reader.block_data(1) {
            Ok(Some(payload)) if payload.as_ref() == b"Hello remote" => {}
            Ok(Some(payload)) => {
                *mismatch = Some(format!("payload extent skewed: {:?}", payload.as_ref()))
            }
            Ok(None) => *mismatch = Some("payload not resident".to_string()),
            Err(e) => *mismatch = Some(format!("payload unreadable: {e}")),
        }

        // The forward-time rewrite inserted a Previous Node block; its
        // extent must decode as an EID from the same bytes.
        let previous_node = (2u64..16).find(|n| {
            reader
                .block(*n)
                .is_some_and(|b| b.block_type == hardy_bpv7::block::Type::PreviousNode)
        });
        match previous_node {
            None => *mismatch = Some("no Previous Node block after the rewrite".to_string()),
            Some(n) => {
                if let Err(e) = reader.extract::<hardy_bpv7::eid::Eid>(n) {
                    *mismatch = Some(format!("Previous Node extent skewed: {e}"));
                }
            }
        }

        hardy_bpa::filter::Verdict::Continue(())
    }
}

/// Forwarding rewrites extension blocks (Previous Node insertion shifts every
/// later block), and Egress filters receive (bundle, data) as a consistent
/// pair: the Bundle's extents must index the rewritten bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn egress_filter_sees_consistent_extents() {
    let mismatch = Arc::new(Mutex::new(None));
    let mut pack = hardy_bpa::filter::pack::FilterPack::new("test");
    pack.egress_verifier(
        "extent-check",
        ExtentCheckVerifier {
            mismatch: mismatch.clone(),
        },
    );
    let bpa = Bpa::builder().add_filters(pack).build().await.unwrap();
    bpa.start(false).await;

    // Register CLA and add a peer for the remote node (ipn:0.2)
    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla("test".to_string(), cla.clone(), None, None)
        .await
        .unwrap();

    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(peer_addr, &[remote_node])
        .await
        .unwrap();

    // Register an application and send a bundle to the remote node — a
    // locally-originated bundle has no Previous Node block, so the
    // forward-time rewrite inserts one and shifts the payload extent
    let (app, _app_rx) = TestApp::new();
    bpa.register_application(Service::Ipn(42), app.clone())
        .await
        .unwrap();
    app.sink
        .get()
        .unwrap()
        .send(
            "ipn:0.2.99".parse().unwrap(),
            Bytes::from_static(b"Hello remote"),
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
    .expect("Timeout waiting for forwarded bundle")
    .expect("Channel closed");

    assert_eq!(
        *mismatch.lock().unwrap(),
        None,
        "Egress filter saw an inconsistent (bundle, data) pair"
    );

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// Failing Application — always returns Err from on_deliver
// ---------------------------------------------------------------------------

struct FailingApp {
    sink: hardy_async::sync::spin::Once<Box<dyn services::ApplicationSink>>,
    /// Fires once on_deliver completes (after constructing the Err), so a
    /// waiter observes the point at which the dispatcher's Err-handling
    /// branch is about to run rather than the point at which it started.
    completed_tx: flume::Sender<()>,
}

impl FailingApp {
    fn new() -> (Arc<Self>, flume::Receiver<()>) {
        let (tx, rx) = flume::bounded(16);
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                completed_tx: tx,
            }),
            rx,
        )
    }
}

#[async_trait]
impl services::Application for FailingApp {
    async fn on_register(&self, _source: &Eid, sink: Box<dyn services::ApplicationSink>) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn on_deliver(
        &self,
        _bundle_id: &Id,
        _expiry: time::OffsetDateTime,
        _ack_requested: bool,
        _total_len: u64,
        _stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        let err = services::Error::Internal("test: simulated delivery failure".into());
        let _ = self.completed_tx.send(());
        Err(err)
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
// INT-BPA-05: dispatcher tolerates on_deliver returning Err
// ---------------------------------------------------------------------------

/// When `on_deliver` returns `Err`, the dispatcher must preserve the bundle
/// as `WaitingForService`, not report it delivered and delete it. Proven
/// end-to-end: after the failing receiver rejects the bundle, a fresh working
/// receiver registered on the same service id must have it re-delivered. A
/// regression to the old drop-on-error behaviour makes the re-delivery time
/// out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatcher_handles_on_deliver_err() {
    let bpa = Bpa::builder().build().await.unwrap();
    bpa.start(false).await;

    let (sender, _sender_rx) = TestApp::new();
    bpa.register_application(Service::Ipn(43), sender.clone())
        .await
        .unwrap();

    let (failing, completed_rx) = FailingApp::new();
    let receiver_eid = bpa
        .register_application(Service::Ipn(42), failing.clone())
        .await
        .unwrap();

    sender
        .sink
        .get()
        .unwrap()
        .send(
            receiver_eid,
            Bytes::from_static(b"payload"),
            Duration::from_secs(3600),
            None,
        )
        .await
        .unwrap();

    // The failing receiver rejects the bundle from within on_deliver.
    // Event-driven wait; the timeout only bounds a regression.
    tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        completed_rx.recv_async(),
    )
    .await
    .expect("dispatcher must call on_deliver")
    .unwrap();

    // Swap in a working receiver on the same service id — no quiesce
    // needed: the failed delivery's park re-checks the routing snapshot, so
    // whether it lands before this registration's WaitingForService poll or
    // after it, the bundle re-enters dispatch and reaches the new receiver.
    failing.sink.get().unwrap().unregister().await;
    let (receiver, received_rx) = TestApp::new();
    bpa.register_application(Service::Ipn(42), receiver.clone())
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
// Deferring CLA — answers Accepted for the first N offers, Sent afterwards,
// and reports every offered bundle id to the test
// ---------------------------------------------------------------------------

struct DeferringCla {
    sink: hardy_async::sync::spin::Once<Box<dyn cla::Sink>>,
    offers_tx: flume::Sender<Id>,
    remaining_accepts: AtomicUsize,
}

impl DeferringCla {
    fn new(accepts: usize) -> (Arc<Self>, flume::Receiver<Id>) {
        let (tx, rx) = flume::bounded(16);
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                offers_tx: tx,
                remaining_accepts: AtomicUsize::new(accepts),
            }),
            rx,
        )
    }

    fn sink(&self) -> &dyn cla::Sink {
        self.sink.get().unwrap().as_ref()
    }
}

#[async_trait]
impl cla::Cla for DeferringCla {
    fn lane_count(&self) -> Option<NonZeroU32> {
        None
    }

    async fn on_register(
        &self,
        sink: Box<dyn cla::Sink>,
        _node_ids: &[NodeId],
        _max_bundle_size: core::num::NonZeroU64,
    ) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn forward(
        &self,
        _queue: Option<u32>,
        _cla_addr: &cla::ClaAddress,
        bundle_id: &Id,
        _total_len: u64,
        _stream: &mut dyn Receiver<cla::Segment>,
    ) -> cla::Result<cla::ForwardBundleResult> {
        let _ = self.offers_tx.send(bundle_id.clone());
        if self
            .remaining_accepts
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            Ok(cla::ForwardBundleResult::Accepted)
        } else {
            Ok(cla::ForwardBundleResult::Sent)
        }
    }
}

// Helper: BPA with a DeferringCla registered and a peer for ipn:0.<node>
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
    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    let (cla, offers_rx) = DeferringCla::new(accepts);
    bpa.register_cla(
        format!("deferring-{peer_node_number}"),
        cla.clone(),
        None,
        None,
    )
    .await
    .unwrap();
    cla.sink()
        .add_peer(
            cla::ClaAddress::Private(format!("peer-{peer_node_number}").into_bytes().into()),
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
// INT-BPA-07: deferred outcome — Failed re-enters dispatch per-bundle
// ---------------------------------------------------------------------------

/// A transfer answered `Accepted` whose outcome is reported `Failed` gets a
/// fresh routing decision and is re-offered to the CLA; the bundle is never
/// dropped.
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
        .transfer_outcome(&id, cla::TransferOutcome::Failed)
        .await
        .unwrap();

    // The failed transfer re-enters dispatch and is re-offered (the route is
    // unchanged); the second offer is answered Sent by the mock.
    let id2 = expect_offer(&offers_rx).await;
    assert_eq!(id, id2, "Re-offer must be the same bundle");

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// INT-BPA-08: deferred outcome — Completed resolves the transfer
// ---------------------------------------------------------------------------

/// A transfer answered `Accepted` whose outcome is reported `Completed` is
/// complete: no re-offer, a late duplicate outcome is ignored, and the
/// tombstone dedups a re-arrival of the same bundle.
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
        .transfer_outcome(&id, cla::TransferOutcome::Completed)
        .await
        .unwrap();

    // A late duplicate outcome is ignored, not honoured twice.
    cla.sink()
        .transfer_outcome(&id, cla::TransferOutcome::Failed)
        .await
        .unwrap();
    expect_no_offer(&offers_rx).await;

    // The completed bundle was deleted with a tombstone: a re-arrival of the
    // same bundle is dropped as a duplicate rather than re-forwarded.
    assert_eq!(
        cla.sink().dispatch(None, None, &mut data).await.unwrap(),
        cla::Acceptance::Accepted
    );
    expect_no_offer(&offers_rx).await;

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// INT-BPA-09: deferred outcome — peer removal resolves outcome-unknown
// ---------------------------------------------------------------------------

/// Removing the peer while a transfer awaits its outcome resolves it as
/// outcome-unknown: the bundle returns to Waiting and is re-offered when the
/// peer comes back.
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

    let peer_addr = cla::ClaAddress::Private("peer-2".as_bytes().into());
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

    // The unresolved transfer went back to Waiting on peer removal, and the
    // re-added peer's route re-dispatches it.
    let id2 = expect_offer(&offers_rx).await;
    assert_eq!(id, id2, "Re-offer must be the same bundle");

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// INT-BPA-10: deferred outcome — only the owning CLA may resolve a transfer
// ---------------------------------------------------------------------------

/// An outcome reported by a CLA that does not own the transfer's peer is
/// ignored; the owning CLA's subsequent outcome is still honoured. Outcomes
/// for unknown bundles are ignored without error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_outcome_ignores_wrong_cla() {
    let (bpa, cla_a, offers_a) = deferring_setup(1, 2).await;

    // A second CLA with its own peer on a different node
    let (cla_b, offers_b) = DeferringCla::new(0);
    bpa.register_cla("deferring-b".to_string(), cla_b.clone(), None, None)
        .await
        .unwrap();
    cla_b
        .sink()
        .add_peer(
            cla::ClaAddress::Private("peer-b".as_bytes().into()),
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
        .transfer_outcome(&unknown, cla::TransferOutcome::Completed)
        .await
        .unwrap();

    // CLA B does not own the transfer's peer: its outcome is ignored.
    cla_b
        .sink()
        .transfer_outcome(&id, cla::TransferOutcome::Completed)
        .await
        .unwrap();
    expect_no_offer(&offers_a).await;

    // The owning CLA's outcome is still honoured.
    cla_a
        .sink()
        .transfer_outcome(&id, cla::TransferOutcome::Failed)
        .await
        .unwrap();
    let id2 = expect_offer(&offers_a).await;
    assert_eq!(id, id2, "Re-offer must be the same bundle");
    expect_no_offer(&offers_b).await;

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// INT-BPA-11: expiry during a CLA-owned transfer is covered by
// tests/forward_expiry.rs — the reaper defers a ForwardAckPending bundle,
// and the outcome resolves it (a failed transfer expires at the dispatch
// expiry checkpoint).
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Blocking CLA — holds the first forward open until released, then fails it
// synchronously; every subsequent offer is answered Sent. All offers are
// recorded.
// ---------------------------------------------------------------------------

struct BlockingCla {
    sink: hardy_async::sync::spin::Once<Box<dyn cla::Sink>>,
    offered_tx: flume::Sender<Id>,
    release_rx: flume::Receiver<()>,
    first: AtomicBool,
}

impl BlockingCla {
    fn new() -> (Arc<Self>, flume::Receiver<Id>, flume::Sender<()>) {
        let (offered_tx, offered_rx) = flume::bounded(16);
        let (release_tx, release_rx) = flume::bounded(1);
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                offered_tx,
                release_rx,
                first: AtomicBool::new(true),
            }),
            offered_rx,
            release_tx,
        )
    }
}

#[async_trait]
impl cla::Cla for BlockingCla {
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
        _queue: Option<u32>,
        _cla_addr: &cla::ClaAddress,
        bundle_id: &Id,
        _total_len: u64,
        _stream: &mut dyn Receiver<Segment>,
    ) -> cla::Result<cla::ForwardBundleResult> {
        let _ = self.offered_tx.send_async(bundle_id.clone()).await;
        if self.first.swap(false, Ordering::SeqCst) {
            let _ = self.release_rx.recv_async().await;
            return Err(cla::Error::StreamCancelled);
        }
        Ok(cla::ForwardBundleResult::Sent)
    }
}

/// A route event that lands while a transfer is in flight is not missed:
/// the event's poll cannot see the bundle (claimed ForwardAckPending), but
/// the synchronous failure's park re-checks the routing snapshot captured
/// when the flight began and re-enters dispatch — the only path to the
/// second offer, since no further route events occur.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forward_failure_park_recheck_redispatches() {
    let bpa = Bpa::builder().build().await.unwrap();
    bpa.start(false).await;

    let (cla, offered_rx, release_tx) = BlockingCla::new();
    bpa.register_cla("blocking".to_string(), cla.clone(), None, None)
        .await
        .unwrap();
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(
            cla::ClaAddress::Private("peer-a".as_bytes().into()),
            from_ref(&remote_node),
        )
        .await
        .unwrap();

    let (app, _app_rx) = TestApp::new();
    bpa.register_application(Service::Ipn(90), app.clone())
        .await
        .unwrap();
    let id = app
        .sink
        .get()
        .unwrap()
        .send(
            "ipn:0.2.7".parse().unwrap(),
            Bytes::from_static(b"recheck"),
            Duration::from_secs(3600),
            None,
        )
        .await
        .unwrap();

    // The transfer is in flight, held open by the CLA...
    // (every timeout below only bounds a regression)
    let offered =
        tokio::time::timeout(tokio::time::Duration::from_secs(5), offered_rx.recv_async())
            .await
            .expect("Timeout waiting for the first offer")
            .unwrap();
    assert_eq!(offered, id);

    // ...when a second peer for the same node appears. Its route event
    // polls Waiting bundles and cannot see this one.
    cla.sink
        .get()
        .unwrap()
        .add_peer(
            cla::ClaAddress::Private("peer-b".as_bytes().into()),
            from_ref(&remote_node),
        )
        .await
        .unwrap();

    // Release: the synchronous failure parks the bundle; the park detects
    // the mid-flight routing change and re-dispatches.
    release_tx.send(()).expect("CLA gone");
    let offered =
        tokio::time::timeout(tokio::time::Duration::from_secs(5), offered_rx.recv_async())
            .await
            .expect("The park's recheck must re-dispatch the bundle")
            .unwrap();
    assert_eq!(offered, id);

    bpa.shutdown().await;
}

/// A synchronous forward failure must not resurrect a bundle that was
/// resolved while its transfer was in flight: the failure's Waiting park is
/// conditional and loses against the terminal claim.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forward_failure_never_resurrects_resolved_bundle() {
    let metadata_store = Arc::new(MetadataMemStorage::new(None));
    let bpa = Bpa::builder()
        .metadata_storage(metadata_store.clone())
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    let (cla, offered_rx, release_tx) = BlockingCla::new();
    bpa.register_cla("blocking".to_string(), cla.clone(), None, None)
        .await
        .unwrap();
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });
    cla.sink
        .get()
        .unwrap()
        .add_peer(
            cla::ClaAddress::Private("peer-a".as_bytes().into()),
            from_ref(&remote_node),
        )
        .await
        .unwrap();

    let (app, _app_rx) = TestApp::new();
    bpa.register_application(Service::Ipn(91), app.clone())
        .await
        .unwrap();
    let id = app
        .sink
        .get()
        .unwrap()
        .send(
            "ipn:0.2.7".parse().unwrap(),
            Bytes::from_static(b"resolve me"),
            Duration::from_secs(3600),
            None,
        )
        .await
        .unwrap();

    // The transfer is in flight (the timeout only bounds a regression)...
    let offered =
        tokio::time::timeout(tokio::time::Duration::from_secs(5), offered_rx.recv_async())
            .await
            .expect("Timeout waiting for the offer")
            .unwrap();
    assert_eq!(offered, id);

    // ...when the bundle is resolved, exactly as the reaper's terminal
    // claim would leave it (delete wins every race).
    metadata_store.tombstone(&id).await.unwrap();

    // Release: the failure's park must lose against the tombstone.
    // shutdown() is the barrier — it joins the egress pollers, so the park
    // has fully run by the time it returns; no quiet window is involved.
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
// The config-gated RFC 9171 validity checks at the pre-drain gate
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
        cla.sink
            .get()
            .unwrap()
            .dispatch(None, None, &mut rx)
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );
    producer.await.unwrap();
}

/// Builds a strict-or-relaxed BPA around a local application at ipn:0.1.42,
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
    let bpa = configure(Bpa::builder().node_ids(node_ids))
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    let (app, app_rx) = TestApp::new();
    bpa.register_application(Service::Ipn(42), app.clone())
        .await
        .unwrap();

    let (cla, _forwarded_rx) = PipelineCla::new();
    bpa.register_cla("test".to_string(), cla.clone(), None, None)
        .await
        .unwrap();

    (bpa, cla, app_rx)
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

/// RFC 9171 §4.3.1: with the default strict config, a primary block with
/// neither CRC nor BIB coverage is rejected at the pre-drain gate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_rejects_unprotected_primary_block() {
    let (bpa, cla, app_rx) = gate_fixture(|b| b).await;

    dispatch_inbound(&cla, unprotected_primary_bundle()).await;

    // Shutdown is the barrier: an admitted bundle would have completed
    // delivery before it returns.
    bpa.shutdown().await;
    assert!(
        app_rx.is_empty(),
        "an unprotected primary block must be rejected at the gate"
    );
}

/// `primary_block_integrity(false)` relaxes the §4.3.1 check: the same
/// bundle is admitted and delivered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relaxed_gate_admits_unprotected_primary_block() {
    let (bpa, cla, app_rx) = gate_fixture(|b| b.primary_block_integrity(false)).await;

    dispatch_inbound(&cla, unprotected_primary_bundle()).await;

    // Event-driven wait; the timeout only bounds a regression.
    let (_, payload) =
        tokio::time::timeout(tokio::time::Duration::from_secs(5), app_rx.recv_async())
            .await
            .expect("Timeout waiting for delivery")
            .expect("Channel closed");
    assert_eq!(payload.as_ref(), b"no integrity");

    bpa.shutdown().await;
}

/// RFC 9171 §4.4.2: with the default strict config, a clockless bundle
/// without a Bundle Age block is rejected at the pre-drain gate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_rejects_clockless_bundle_without_age() {
    let (bpa, cla, app_rx) = gate_fixture(|b| b).await;

    dispatch_inbound(&cla, clockless_ageless_bundle()).await;

    // Shutdown is the barrier: an admitted bundle would have completed
    // delivery before it returns.
    bpa.shutdown().await;
    assert!(
        app_rx.is_empty(),
        "a clockless bundle without a Bundle Age block must be rejected at the gate"
    );
}

/// `bundle_age_required(false)` relaxes the §4.4.2 check: the same bundle
/// is admitted and delivered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relaxed_gate_admits_clockless_bundle_without_age() {
    let (bpa, cla, app_rx) = gate_fixture(|b| b.bundle_age_required(false)).await;

    dispatch_inbound(&cla, clockless_ageless_bundle()).await;

    // Event-driven wait; the timeout only bounds a regression.
    let (_, payload) =
        tokio::time::timeout(tokio::time::Duration::from_secs(5), app_rx.recv_async())
            .await
            .expect("Timeout waiting for delivery")
            .expect("Channel closed");
    assert_eq!(payload.as_ref(), b"no clock");

    bpa.shutdown().await;
}

// A peer removed while its construction is still in flight leaves nothing
// behind (whole-codebase review #14): add_peer claims the address, builds
// the peer complete, publishes, then re-checks its claim before installing
// RIB entries — a concurrent remove wins the claim, and the half-added peer
// is withdrawn (add_peer reports false). The policy's controller
// construction is the interception point: it parks until released, and a
// fresh add on the same address afterwards succeeds cleanly.
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
            queues: std::collections::HashMap<Option<u32>, Arc<dyn policy::EgressQueue>>,
        ) -> Arc<dyn policy::FlowController> {
            let _ = self.entered_tx.send(());
            // Parked until the test drops the release sender; the peer is
            // mid-construction for exactly this window.
            let _ = self.release_rx.recv_async().await;
            Arc::new(DefaultController {
                queue: queues.get(&None).expect("next-free queue exists").clone(),
            })
        }
    }

    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids = NodeIds::try_from([NodeId::Ipn(node_id)].as_slice()).unwrap();
    let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
    bpa.start(false).await;

    let (entered_tx, entered_rx) = flume::unbounded();
    let (release_tx, release_rx) = flume::bounded::<()>(1);
    let (cla, _forwarded_rx) = PipelineCla::new();
    bpa.register_cla(
        "test".to_string(),
        cla.clone(),
        Some(Arc::new(ParkedPolicy {
            entered_tx,
            release_rx,
        })),
        None,
    )
    .await
    .unwrap();

    let peer_addr = cla::ClaAddress::Private("peer".as_bytes().into());
    let remote_node = NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 2,
    });

    // The add parks in controller construction...
    let add = {
        let cla = cla.clone();
        let peer_addr = peer_addr.clone();
        let remote_node = remote_node.clone();
        tokio::spawn(async move {
            cla.sink
                .get()
                .unwrap()
                .add_peer(peer_addr, from_ref(&remote_node))
                .await
        })
    };
    // Event-driven wait; the timeout only bounds a regression.
    tokio::time::timeout(tokio::time::Duration::from_secs(5), entered_rx.recv_async())
        .await
        .expect("Timeout waiting for controller construction to start")
        .expect("Policy gone");

    // ...and the removal wins the claim while it is parked.
    assert!(
        cla.sink
            .get()
            .unwrap()
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
            .get()
            .unwrap()
            .add_peer(peer_addr, from_ref(&remote_node))
            .await
            .expect("add_peer failed"),
        "a fresh add on the freed address succeeds"
    );

    bpa.shutdown().await;
}
