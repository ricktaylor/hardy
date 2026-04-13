//! BPA Pipeline Integration Tests
//!
//! These tests verify end-to-end bundle processing through the BPA,
//! covering the component test plan (PLAN-BPA-01) Suites A and B.

use hardy_bpa::bpa::{Bpa, BpaRegistration};
use hardy_bpa::cla;
use hardy_bpa::services;
use hardy_bpa::{Bytes, async_trait};
use hardy_bpv7::eid::{Eid, IpnNodeId, NodeId};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Test CLA — captures forwarded bundles via a channel
// ---------------------------------------------------------------------------

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
    async fn on_register(&self, sink: Box<dyn cla::Sink>, _node_ids: &[NodeId]) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn forward(
        &self,
        _queue: Option<u32>,
        _cla_addr: &cla::ClaAddress,
        bundle: Bytes,
    ) -> cla::Result<cla::ForwardBundleResult> {
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

    async fn on_receive(
        &self,
        source: Eid,
        _expiry: time::OffsetDateTime,
        _ack_requested: bool,
        payload: Bytes,
    ) {
        let _ = self.received_tx.send((source, payload));
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

    async fn on_receive(&self, data: Bytes, _expiry: time::OffsetDateTime) {
        let Ok(parsed) = hardy_bpv7::bundle::ParsedBundle::parse(&data, hardy_bpv7::bpsec::no_keys)
        else {
            return;
        };

        // Swap source and destination via Editor
        let Ok(editor) = hardy_bpv7::editor::Editor::new(&parsed.bundle, &data)
            .with_source(parsed.bundle.destination.clone())
            .map_err(|(_, e)| e)
        else {
            return;
        };
        let Ok(editor) = editor
            .with_destination(parsed.bundle.id.source.clone())
            .map_err(|(_, e)| e)
        else {
            return;
        };
        let Ok(reply_data) = editor.rebuild() else {
            return;
        };

        if let Some(sink) = self.sink.get() {
            let _ = sink.send(reply_data.into()).await;
        }
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
    async fn on_register(&self, sink: Box<dyn cla::Sink>, _node_ids: &[NodeId]) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn forward(
        &self,
        _queue: Option<u32>,
        _cla_addr: &cla::ClaAddress,
        _bundle: Bytes,
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
    if let Ok(cpuinfo) = fs::read_to_string("/proc/cpuinfo") {
        if let Some(model) = cpuinfo
            .lines()
            .find(|l| l.starts_with("model name"))
            .and_then(|l| l.split(':').nth(1))
        {
            eprintln!("CPU: {}", model.trim());
        }
    }

    // Logical cores
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    eprintln!("Cores: {cores}");

    // Total RAM
    if let Ok(meminfo) = fs::read_to_string("/proc/meminfo") {
        if let Some(total) = meminfo
            .lines()
            .find(|l| l.starts_with("MemTotal"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
        {
            eprintln!("RAM: {} GB", total / 1_048_576);
        }
    }

    // OS
    if let Ok(release) = fs::read_to_string("/etc/os-release") {
        if let Some(pretty) = release
            .lines()
            .find(|l| l.starts_with("PRETTY_NAME"))
            .and_then(|l| l.split('=').nth(1))
        {
            eprintln!("OS: {}", pretty.trim_matches('"'));
        }
    }

    eprintln!("Arch: {}", std::env::consts::ARCH);

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
    let (_, data) = hardy_bpv7::builder::Builder::new(source.clone(), destination.clone())
        .with_payload(std::borrow::Cow::Borrowed(payload))
        .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
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
    let bpa = Bpa::builder().build();
    bpa.start(false);

    // Register CLA and add a peer for the remote node (ipn:0.2)
    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla("test".to_string(), None, cla.clone(), None)
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
        .register_application(Some(hardy_bpv7::eid::Service::Ipn(42)), app.clone())
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
            core::time::Duration::from_secs(3600),
            None,
        )
        .await
        .unwrap();

    // The CLA should forward the bundle
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for forwarded bundle")
    .expect("Channel closed");

    // Parse and verify the forwarded bundle
    let parsed = hardy_bpv7::bundle::ParsedBundle::parse(&forwarded, hardy_bpv7::bpsec::no_keys)
        .expect("Failed to parse forwarded bundle");

    assert_eq!(parsed.bundle.id.source, source_eid);
    assert_eq!(parsed.bundle.destination, dest);

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
    let node_ids =
        hardy_bpa::node_ids::NodeIds::try_from([NodeId::Ipn(node_id.clone())].as_slice()).unwrap();

    let bpa = Bpa::builder().node_ids(node_ids).build();
    bpa.start(false);

    // Register echo service on service number 7
    let echo = EchoService::new();
    bpa.register_service(Some(hardy_bpv7::eid::Service::Ipn(7)), echo)
        .await
        .unwrap();

    // Register CLA with a peer for the "remote" node (ipn:0.2)
    let (cla, forwarded_rx) = PipelineCla::new();
    bpa.register_cla("test".to_string(), None, cla.clone(), None)
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
        .add_peer(peer_addr, std::slice::from_ref(&remote_node))
        .await
        .unwrap();

    // Build an inbound bundle: from remote node, to our echo service
    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let echo_dest: Eid = "ipn:0.1.7".parse().unwrap();
    let inbound = build_bundle(&remote_source, &echo_dest, b"ping");

    // Dispatch it as if received from the CLA
    cla.sink
        .get()
        .unwrap()
        .dispatch(inbound, Some(&remote_node), None)
        .await
        .unwrap();

    // The echo service should reflect the bundle back:
    // source=ipn:0.1.7 (echo), dest=ipn:0.2.1 (remote)
    // BPA routes to CLA peer (ipn:0.2.*)
    let forwarded = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        forwarded_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for echo reply")
    .expect("Channel closed");

    // Parse and verify the echo reply
    let parsed = hardy_bpv7::bundle::ParsedBundle::parse(&forwarded, hardy_bpv7::bpsec::no_keys)
        .expect("Failed to parse echo reply");

    // Source and destination should be swapped
    assert_eq!(
        parsed.bundle.destination, remote_source,
        "Echo reply destination should be the original source"
    );
    assert_eq!(
        parsed.bundle.id.source, echo_dest,
        "Echo reply source should be the echo service"
    );

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// INT-BPA-03: Local Delivery
// ---------------------------------------------------------------------------

/// A bundle dispatched via CLA addressed to a local application is
/// delivered to that application's on_receive callback.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_delivery() {
    let node_id = IpnNodeId {
        allocator_id: 0,
        node_number: 1,
    };
    let node_ids =
        hardy_bpa::node_ids::NodeIds::try_from([NodeId::Ipn(node_id.clone())].as_slice()).unwrap();

    let bpa = Bpa::builder().node_ids(node_ids).build();
    bpa.start(false);

    // Register an application on service number 42
    let (app, app_rx) = TestApp::new();
    bpa.register_application(Some(hardy_bpv7::eid::Service::Ipn(42)), app.clone())
        .await
        .unwrap();

    // Register CLA (needed for dispatch)
    let (cla, _forwarded_rx) = PipelineCla::new();
    bpa.register_cla("test".to_string(), None, cla.clone(), None)
        .await
        .unwrap();

    // Build an inbound bundle addressed to our local application
    let remote_source: Eid = "ipn:0.2.1".parse().unwrap();
    let local_dest: Eid = "ipn:0.1.42".parse().unwrap();
    let inbound = build_bundle(&remote_source, &local_dest, b"Hello local");

    // Dispatch via CLA
    cla.sink
        .get()
        .unwrap()
        .dispatch(inbound, None, None)
        .await
        .unwrap();

    // Application should receive the payload
    let (source, payload) =
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
    let node_ids =
        hardy_bpa::node_ids::NodeIds::try_from([NodeId::Ipn(node_id.clone())].as_slice()).unwrap();

    let bpa = Bpa::builder().node_ids(node_ids).build();
    bpa.start(false);

    let (cla, arrival_rx) = TimedCla::new();
    bpa.register_cla("test".to_string(), None, cla.clone(), None)
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
        .add_peer(peer_addr, std::slice::from_ref(&remote_node))
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
    for (i, bundle) in warmup_bundles.into_iter().enumerate() {
        cla.sink
            .get()
            .unwrap()
            .dispatch(bundle, None, None)
            .await
            .unwrap();
        tokio::time::timeout(tokio::time::Duration::from_secs(5), arrival_rx.recv_async())
            .await
            .unwrap_or_else(|_| panic!("Timeout waiting for warmup bundle {i}"))
            .unwrap();
    }

    // Measure — serial dispatch+receive to avoid backpressure and duplicate issues.
    // Each bundle is dispatched and received before the next is sent.
    let start = tokio::time::Instant::now();
    let mut last_arrival = start;
    for (i, bundle) in test_bundles.into_iter().enumerate() {
        cla.sink
            .get()
            .unwrap()
            .dispatch(bundle, None, None)
            .await
            .unwrap();
        last_arrival =
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

    // REQ-13: >1000 bundles/sec (in-memory, no I/O)
    assert!(
        bundles_per_sec > 1000.0,
        "Throughput {bundles_per_sec:.0} bundles/sec below REQ-13 target of 1000"
    );

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
    let node_ids =
        hardy_bpa::node_ids::NodeIds::try_from([NodeId::Ipn(node_id.clone())].as_slice()).unwrap();

    let bpa = Bpa::builder().node_ids(node_ids).build();
    bpa.start(false);

    let (cla, arrival_rx) = TimedCla::new();
    bpa.register_cla("test".to_string(), None, cla.clone(), None)
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
        .add_peer(peer_addr, std::slice::from_ref(&remote_node))
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
    for (i, bundle) in warmup_bundles.into_iter().enumerate() {
        cla.sink
            .get()
            .unwrap()
            .dispatch(bundle, None, None)
            .await
            .unwrap();
        tokio::time::timeout(tokio::time::Duration::from_secs(5), arrival_rx.recv_async())
            .await
            .unwrap_or_else(|_| panic!("Timeout waiting for warmup bundle {i}"))
            .unwrap();
    }

    // Measure individual dispatch-to-forward latencies.
    // The arrival time is sampled inside forward(), so we measure the
    // actual pipeline processing time, not the channel wait.
    let mut latencies = Vec::with_capacity(count);

    for (i, bundle) in test_bundles.into_iter().enumerate() {
        let dispatched = tokio::time::Instant::now();
        cla.sink
            .get()
            .unwrap()
            .dispatch(bundle, None, None)
            .await
            .unwrap();
        let arrived =
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
