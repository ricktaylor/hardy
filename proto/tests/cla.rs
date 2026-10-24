//! CLA client proxy tests (CLA-CLI-01 through CLA-CLI-09).
//!
//! Verify the CLA client correctly maps Rust trait calls to cla.proto
//! messages via the gRPC proxy.

mod common;

use common::MockBpa;
use core::num::{NonZeroU32, NonZeroU64};
use hardy_bpa::async_trait;
use hardy_bpa::bpa::BpaRegistration;
use hardy_bpa::cla::{self, ClaAddress, ClaInit, ForwardBundleResult, Lanes};
use hardy_bpv7::eid::NodeId;
use hardy_proto::client::RemoteBpa;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

// A mock CLA that records lifecycle callbacks and forward requests.
struct MockCla {
    registered: AtomicBool,
    sink: hardy_async::sync::spin::Mutex<Option<Box<dyn cla::Sink>>>,
    // The effective cap on_register delivered (outer None = not called).
    max_bundle_size: hardy_async::sync::spin::Mutex<Option<Option<NonZeroU64>>>,
    forwarded: AtomicBool,
    // Answer Accepted (deferring the outcome) instead of Sent.
    defer: bool,
}

impl MockCla {
    fn new() -> Self {
        Self {
            registered: AtomicBool::new(false),
            sink: hardy_async::sync::spin::Mutex::new(None),
            max_bundle_size: hardy_async::sync::spin::Mutex::new(None),
            forwarded: AtomicBool::new(false),
            defer: false,
        }
    }

    fn new_deferring() -> Self {
        Self {
            defer: true,
            ..Self::new()
        }
    }

    fn take_sink(&self) -> Option<Box<dyn cla::Sink>> {
        self.sink.lock().take()
    }

    fn is_forwarded(&self) -> bool {
        self.forwarded.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl cla::Cla for MockCla {
    async fn on_register(
        &self,
        sink: Box<dyn cla::Sink>,
        _node_ids: &[NodeId],
        max_bundle_size: Option<NonZeroU64>,
    ) {
        *self.sink.lock() = Some(sink);
        *self.max_bundle_size.lock() = Some(max_bundle_size);
        self.registered.store(true, Ordering::Relaxed);
    }

    async fn on_unregister(&self) {}

    async fn forward(
        &self,
        _queue: Option<u32>,
        _cla_addr: &ClaAddress,
        _bundle_id: &hardy_bpv7::bundle::Id,
        _total_len: u64,
        _stream: &mut dyn hardy_bpa::stream::Receiver<hardy_bpa::cla::Segment>,
    ) -> cla::Result<ForwardBundleResult> {
        self.forwarded.store(true, Ordering::Relaxed);
        Ok(if self.defer {
            ForwardBundleResult::Accepted
        } else {
            ForwardBundleResult::Sent
        })
    }
}

// CLA-CLI-01: Register CLA, receive node IDs.
#[tokio::test]
async fn cla_cli_01_registration() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["cla"]).await;

    let cla = Arc::new(MockCla::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let node_ids: Vec<NodeId> = remote_bpa
        .register_cla(
            "test-cla".to_string(),
            cla.clone(),
            None,
            ClaInit::default(),
        )
        .await
        .expect("registration should succeed");

    assert!(!node_ids.is_empty(), "should receive at least one node ID");
    assert!(
        cla.registered.load(Ordering::Relaxed),
        "CLA should have received on_register"
    );
    assert!(
        cla.sink.lock().is_some(),
        "CLA should have a sink after registration"
    );

    // Clean up
    drop(cla.take_sink());
    server_tasks.shutdown().await;
}

// CLA-CLI-02: Dispatch bundle from CLA to BPA.
#[tokio::test]
async fn cla_cli_02_dispatch_bundle() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["cla"]).await;

    let cla = Arc::new(MockCla::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _node_ids: Vec<NodeId> = remote_bpa
        .register_cla(
            "test-cla".to_string(),
            cla.clone(),
            None,
            ClaInit::default(),
        )
        .await
        .expect("registration should succeed");

    let sink = cla.take_sink().expect("CLA should have a sink");

    let mut bundle_data = hardy_bpa::Bytes::from_static(b"\x9f\x89\x07\x00\x00\x82\x01\x00");
    assert_eq!(
        sink.dispatch(None, None, &mut bundle_data)
            .await
            .expect("dispatch should succeed"),
        hardy_bpa::cla::Acceptance::Accepted
    );

    // Clean up
    sink.unregister().await;
    server_tasks.shutdown().await;
}

// CLA-CLI-03: Forward bundle from BPA to CLA.
//
// The BPA calls `forward()` on the server-side RemoteCla, which
// proxies the request to the client-side MockCla.
#[tokio::test]
async fn cla_cli_03_forward_bundle() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["cla"]).await;

    let cla = Arc::new(MockCla::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _node_ids: Vec<NodeId> = remote_bpa
        .register_cla(
            "test-cla".to_string(),
            cla.clone(),
            None,
            ClaInit::default(),
        )
        .await
        .expect("registration should succeed");

    assert!(!cla.is_forwarded());

    // The BPA pushes a forward request via the server-side CLA proxy
    let server_cla = bpa
        .last_cla
        .lock()
        .clone()
        .expect("BPA should have the server-side CLA");

    let addr = ClaAddress::Tcp("127.0.0.1:4556".parse().unwrap());
    let mut bundle = hardy_bpa::Bytes::from_static(b"\x9f\x89\x07\x00\x00");
    let total_len = bundle.len() as u64;
    let bundle_id = hardy_bpv7::bundle::Id {
        source: "ipn:0.9.1".parse().unwrap(),
        timestamp: hardy_bpv7::creation_timestamp::CreationTimestamp::now(),
        fragment_info: None,
    };
    let result = server_cla
        .forward(None, &addr, &bundle_id, total_len, &mut bundle)
        .await
        .expect("forward should succeed");

    assert!(
        matches!(result, ForwardBundleResult::Sent),
        "forward should return Sent"
    );
    assert!(
        cla.is_forwarded(),
        "MockCla should have received the forward request"
    );

    // Clean up
    drop(cla.take_sink());
    server_tasks.shutdown().await;
}

// CLA-CLI-04: Add peer.
#[tokio::test]
async fn cla_cli_04_add_peer() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["cla"]).await;

    let cla = Arc::new(MockCla::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _node_ids: Vec<NodeId> = remote_bpa
        .register_cla(
            "test-cla".to_string(),
            cla.clone(),
            None,
            ClaInit::default(),
        )
        .await
        .expect("registration should succeed");

    let sink = cla.take_sink().expect("CLA should have a sink");

    let addr = ClaAddress::Tcp("192.168.1.1:4556".parse().unwrap());
    let peer_node: NodeId = "ipn:2.0".parse().unwrap();
    let added = sink
        .add_peer(addr, &[peer_node])
        .await
        .expect("add_peer should succeed");

    assert!(added, "peer should be added");

    // Clean up
    sink.unregister().await;
    server_tasks.shutdown().await;
}

// CLA-CLI-05: Remove peer.
#[tokio::test]
async fn cla_cli_05_remove_peer() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["cla"]).await;

    let cla = Arc::new(MockCla::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _node_ids: Vec<NodeId> = remote_bpa
        .register_cla(
            "test-cla".to_string(),
            cla.clone(),
            None,
            ClaInit::default(),
        )
        .await
        .expect("registration should succeed");

    let sink = cla.take_sink().expect("CLA should have a sink");

    let addr = ClaAddress::Tcp("192.168.1.1:4556".parse().unwrap());
    let removed = sink
        .remove_peer(&addr)
        .await
        .expect("remove_peer should succeed");

    assert!(removed, "peer should be removed");

    // Clean up
    sink.unregister().await;
    server_tasks.shutdown().await;
}

// CLA-CLI-06: Deferred transfer outcome round trip.
// A CLA answers Accepted to a forward and later resolves the transfer with
// a TransferOutcomeRequest carrying the same bundle id.
#[tokio::test]
async fn cla_cli_06_deferred_outcome() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["cla"]).await;

    let cla = Arc::new(MockCla::new_deferring());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _node_ids: Vec<NodeId> = remote_bpa
        .register_cla(
            "test-cla".to_string(),
            cla.clone(),
            None,
            ClaInit::default(),
        )
        .await
        .expect("registration should succeed");

    let server_cla = bpa
        .last_cla
        .lock()
        .clone()
        .expect("BPA should have the server-side CLA");

    // The forward is answered Accepted: ownership transferred, no outcome yet.
    let addr = ClaAddress::Tcp("127.0.0.1:4556".parse().unwrap());
    let mut bundle = hardy_bpa::Bytes::from_static(b"\x9f\x89\x07\x00\x00");
    let total_len = bundle.len() as u64;
    let bundle_id = hardy_bpv7::bundle::Id {
        source: "ipn:0.9.2".parse().unwrap(),
        timestamp: hardy_bpv7::creation_timestamp::CreationTimestamp::now(),
        fragment_info: None,
    };
    let result = server_cla
        .forward(None, &addr, &bundle_id, total_len, &mut bundle)
        .await
        .expect("forward should succeed");
    assert!(
        matches!(result, ForwardBundleResult::Accepted),
        "forward should return Accepted"
    );
    let sink = bpa
        .last_cla_sink
        .lock()
        .clone()
        .expect("BPA should have the CLA sink");
    assert!(
        sink.outcomes().is_empty(),
        "No outcome should have been reported yet"
    );

    // The CLA resolves the transfer; the outcome arrives at the BPA sink
    // keyed by the same bundle id. The call is correlated, so the outcome is
    // recorded by the time it returns.
    let client_sink = cla.take_sink().expect("CLA should have a sink");
    client_sink
        .transfer_outcome(&bundle_id, cla::TransferOutcome::Completed)
        .await
        .expect("transfer_outcome should succeed");

    assert_eq!(
        sink.outcomes(),
        vec![(bundle_id, cla::TransferOutcome::Completed)],
        "The BPA should have received the outcome"
    );

    // Clean up
    drop(client_sink);
    server_tasks.shutdown().await;
}

// CLA-CLI-07: the registration declarations round-trip the wire: the whole
// `ClaInit` — address type, lane count, and declared limit — reaches the
// BPA's register_cla, and the effective cap the BPA hands to on_register
// comes back in the response and reaches the CLA's on_register. Declared
// and effective caps are distinct values, so a relay that echoes the wrong
// side cannot pass.
#[tokio::test]
async fn cla_cli_07_cla_init_round_trip() {
    let declared = ClaInit {
        address_type: Some(hardy_bpa::cla::ClaAddressType::Tcp),
        lanes: Lanes::Limited(NonZeroU32::new(4).unwrap()),
        max_bundle_size: Some(NonZeroU64::new(1024 * 1024).unwrap()),
    };
    let effective = NonZeroU64::new(512 * 1024).unwrap();

    let bpa = Arc::new(MockBpa::with_effective_max_bundle_size(effective));
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["cla"]).await;

    let cla = Arc::new(MockCla::new());
    RemoteBpa::new(grpc_addr)
        .register_cla("test-cla".to_string(), cla.clone(), None, declared)
        .await
        .expect("registration should succeed");

    assert_eq!(
        *bpa.declared_init.lock(),
        Some(declared),
        "The BPA should receive the CLA's declarations intact"
    );
    assert_eq!(
        *cla.max_bundle_size.lock(),
        Some(Some(effective)),
        "The CLA should receive the BPA's effective cap"
    );

    // Clean up
    drop(cla.take_sink());
    server_tasks.shutdown().await;
}

// CLA-CLI-08: an empty `ClaInit` round-trips as absent in both directions —
// every declaration reaches the BPA as None, and a BPA with no negotiated
// cap reaches the CLA as None (the same decode an old, pre-field BPA's
// response produces).
#[tokio::test]
async fn cla_cli_08_absent_declarations_round_trip_as_none() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["cla"]).await;

    let cla = Arc::new(MockCla::new());
    RemoteBpa::new(grpc_addr)
        .register_cla(
            "test-cla".to_string(),
            cla.clone(),
            None,
            ClaInit::default(),
        )
        .await
        .expect("registration should succeed");

    assert_eq!(
        *bpa.declared_init.lock(),
        Some(ClaInit::default()),
        "The BPA should see no declarations"
    );
    assert_eq!(
        *cla.max_bundle_size.lock(),
        Some(None),
        "The CLA should see no negotiated cap"
    );

    // Clean up
    drop(cla.take_sink());
    server_tasks.shutdown().await;
}

// A remote CLA whose forwards announce themselves and hold until the test
// releases them, recording the most ever in flight at once.
struct HoldingCla {
    sink: hardy_async::sync::spin::Mutex<Option<Box<dyn cla::Sink>>>,
    entered_tx: tokio::sync::mpsc::UnboundedSender<()>,
    release: Arc<tokio::sync::Semaphore>,
    in_flight: std::sync::atomic::AtomicUsize,
    max_in_flight: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl cla::Cla for HoldingCla {
    async fn on_register(
        &self,
        sink: Box<dyn cla::Sink>,
        _node_ids: &[NodeId],
        _max_bundle_size: Option<NonZeroU64>,
    ) {
        *self.sink.lock() = Some(sink);
    }

    async fn on_unregister(&self) {}

    async fn forward(
        &self,
        _lane: Option<u32>,
        _cla_addr: &ClaAddress,
        _bundle_id: &hardy_bpv7::bundle::Id,
        _total_len: u64,
        _stream: &mut dyn hardy_bpa::stream::Receiver<hardy_bpa::cla::Segment>,
    ) -> cla::Result<ForwardBundleResult> {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(now, Ordering::SeqCst);
        let _ = self.entered_tx.send(());
        self.release
            .acquire()
            .await
            .expect("release semaphore open")
            .forget();
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(ForwardBundleResult::Sent)
    }
}

// CLA-CLI-09: a remote CLA declaring more lanes than the client has CPUs
// takes that many forwards at once: the client proxy's handler bound grows
// with the declaration, so a bound sized by CPU count alone cannot cap it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cla_cli_09_declared_lanes_bound_the_client_not_its_cpus() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["cla"]).await;

    let lanes = hardy_async::available_parallelism().get() + 2;
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let cla = Arc::new(HoldingCla {
        sink: hardy_async::sync::spin::Mutex::new(None),
        entered_tx,
        release: release.clone(),
        in_flight: std::sync::atomic::AtomicUsize::new(0),
        max_in_flight: std::sync::atomic::AtomicUsize::new(0),
    });
    RemoteBpa::new(grpc_addr)
        .register_cla(
            "test-cla".to_string(),
            cla.clone(),
            None,
            ClaInit {
                lanes: Lanes::Limited(NonZeroU32::new(u32::try_from(lanes).unwrap()).unwrap()),
                ..Default::default()
            },
        )
        .await
        .expect("registration should succeed");
    let server_cla = bpa
        .last_cla
        .lock()
        .clone()
        .expect("BPA should have the server-side CLA");

    let forwards = (0..lanes)
        .map(|_| {
            let server_cla = server_cla.clone();
            tokio::spawn(async move {
                let mut bundle = hardy_bpa::Bytes::from_static(b"\x9f\x89\x07\x00\x00");
                let total_len = bundle.len() as u64;
                let bundle_id = hardy_bpv7::bundle::Id {
                    source: "ipn:0.9.1".parse().unwrap(),
                    timestamp: hardy_bpv7::creation_timestamp::CreationTimestamp::now(),
                    fragment_info: None,
                };
                server_cla
                    .forward(
                        None,
                        &ClaAddress::Tcp("127.0.0.1:4556".parse().unwrap()),
                        &bundle_id,
                        total_len,
                        &mut bundle,
                    )
                    .await
                    .expect("forward should succeed")
            })
        })
        .collect::<Vec<_>>();

    // Every forward is in flight at the CLA at once. The timeout only
    // bounds a regression: a CPU-count bound would hold the rest back.
    for _ in 0..lanes {
        tokio::time::timeout(std::time::Duration::from_secs(10), entered_rx.recv())
            .await
            .expect("a declared lane was held back")
            .expect("CLA gone");
    }
    assert_eq!(cla.in_flight.load(Ordering::SeqCst), lanes);

    release.add_permits(lanes);
    for forward in forwards {
        assert!(matches!(forward.await.unwrap(), ForwardBundleResult::Sent));
    }
    drop(cla.sink.lock().take());
    server_tasks.shutdown().await;
    assert_eq!(cla.max_in_flight.load(Ordering::SeqCst), lanes);
}
