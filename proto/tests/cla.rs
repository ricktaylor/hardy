//! CLA client proxy tests (CLA-CLI-01 through CLA-CLI-06).
//!
//! The CLA client maps its Rust trait calls onto cla.proto messages,
//! across the gRPC proxy.

mod common;

use common::MockBpa;
use core::num::{NonZeroU32, NonZeroU64};
use hardy_bpa::async_trait;
use hardy_bpa::cla::{
    self, Address, Context, Factory, ForwardOutcome, RegistrarExt, Request, Sink, Terms, factory_fn,
};
use hardy_bpa::node_ids::NodeIds;
use hardy_bpv7::eid::NodeId;
use hardy_proto::client::RemoteBpa;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

// A mock CLA that records the context it was built from and the forward
// requests the BPA pushes at it.
struct MockCla {
    sink: Arc<dyn Sink>,
    node_ids: Arc<NodeIds>,
    terms: Terms,
    forwarded: AtomicBool,
    // Answer Accepted (deferring the outcome) instead of Sent.
    defer: bool,
}

impl MockCla {
    // A mock answering every forward terminally.
    fn factory() -> impl Factory<Component = Self> {
        Self::build(Request::default(), false)
    }

    // A mock declaring `request`, for the registration round trip.
    fn declaring(request: Request) -> impl Factory<Component = Self> {
        Self::build(request, false)
    }

    // A mock answering every forward Accepted, resolving the transfer
    // later through the sink.
    fn deferring() -> impl Factory<Component = Self> {
        Self::build(Request::default(), true)
    }

    fn build(request: Request, defer: bool) -> impl Factory<Component = Self> {
        factory_fn(request, move |cx: Context| {
            Ok(Arc::new(Self {
                sink: cx.sink,
                node_ids: cx.node_ids,
                terms: cx.terms,
                forwarded: AtomicBool::new(false),
                defer,
            }))
        })
    }

    fn is_forwarded(&self) -> bool {
        self.forwarded.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl cla::Cla for MockCla {
    async fn forward(
        &self,
        _queue: Option<u32>,
        _cla_addr: &Address,
        _bundle_id: &hardy_bpv7::bundle::Id,
        _total_len: u64,
        _stream: &mut dyn hardy_bpa::stream::Receiver<hardy_bpa::cla::Segment>,
    ) -> cla::Result<ForwardOutcome> {
        self.forwarded.store(true, Ordering::Relaxed);
        Ok(if self.defer {
            ForwardOutcome::Accepted
        } else {
            ForwardOutcome::Sent
        })
    }
}

// CLA-CLI-01: Register CLA, receive node IDs.
#[tokio::test]
async fn cla_cli_01_registration() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["cla"]).await;

    let cla = RemoteBpa::new(grpc_addr)
        .register_cla("test-cla", MockCla::factory())
        .await
        .expect("registration should succeed");

    assert!(
        !Vec::<NodeId>::from(cla.node_ids.as_ref()).is_empty(),
        "should receive at least one node ID"
    );

    cla.sink.unregister().await;
    server_tasks.shutdown().await;
}

// CLA-CLI-02: Dispatch bundle from CLA to BPA.
#[tokio::test]
async fn cla_cli_02_dispatch_bundle() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["cla"]).await;

    let cla = RemoteBpa::new(grpc_addr)
        .register_cla("test-cla", MockCla::factory())
        .await
        .expect("registration should succeed");

    let mut bundle_data = hardy_bpa::Bytes::from_static(b"\x9f\x89\x07\x00\x00\x82\x01\x00");
    assert_eq!(
        cla.sink
            .dispatch(None, None, &mut bundle_data)
            .await
            .expect("dispatch should succeed"),
        hardy_bpa::cla::Acceptance::Accepted
    );

    cla.sink.unregister().await;
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

    let cla = RemoteBpa::new(grpc_addr)
        .register_cla("test-cla", MockCla::factory())
        .await
        .expect("registration should succeed");

    assert!(!cla.is_forwarded());

    // The BPA pushes a forward request via the server-side CLA proxy
    let server_cla = bpa
        .last_cla
        .lock()
        .clone()
        .expect("BPA should have the server-side CLA");

    let addr = Address::Tcp("127.0.0.1:4556".parse().unwrap());
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
        matches!(result, ForwardOutcome::Sent),
        "forward should return Sent"
    );
    assert!(
        cla.is_forwarded(),
        "MockCla should have received the forward request"
    );

    cla.sink.unregister().await;
    server_tasks.shutdown().await;
}

// CLA-CLI-04: Add peer.
#[tokio::test]
async fn cla_cli_04_add_peer() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["cla"]).await;

    let cla = RemoteBpa::new(grpc_addr)
        .register_cla("test-cla", MockCla::factory())
        .await
        .expect("registration should succeed");

    let addr = Address::Tcp("192.168.1.1:4556".parse().unwrap());
    let peer_node: NodeId = "ipn:2.0".parse().unwrap();
    let added = cla
        .sink
        .add_peer(addr, &[peer_node])
        .await
        .expect("add_peer should succeed");

    assert!(added, "peer should be added");

    cla.sink.unregister().await;
    server_tasks.shutdown().await;
}

// CLA-CLI-05: Remove peer.
#[tokio::test]
async fn cla_cli_05_remove_peer() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["cla"]).await;

    let cla = RemoteBpa::new(grpc_addr)
        .register_cla("test-cla", MockCla::factory())
        .await
        .expect("registration should succeed");

    let addr = Address::Tcp("192.168.1.1:4556".parse().unwrap());
    let removed = cla
        .sink
        .remove_peer(&addr)
        .await
        .expect("remove_peer should succeed");

    assert!(removed, "peer should be removed");

    cla.sink.unregister().await;
    server_tasks.shutdown().await;
}

// CLA-CLI-06: Deferred transfer outcome round trip.
// A CLA answers Accepted to a forward and later resolves the transfer with
// a TransferOutcomeRequest carrying the same bundle id.
#[tokio::test]
async fn cla_cli_06_deferred_outcome() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["cla"]).await;

    let cla = RemoteBpa::new(grpc_addr)
        .register_cla("test-cla", MockCla::deferring())
        .await
        .expect("registration should succeed");

    let server_cla = bpa
        .last_cla
        .lock()
        .clone()
        .expect("BPA should have the server-side CLA");

    // The forward is answered Accepted: ownership transferred, no outcome yet.
    let addr = Address::Tcp("127.0.0.1:4556".parse().unwrap());
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
        matches!(result, ForwardOutcome::Accepted),
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
    cla.sink
        .report_transfer(&bundle_id, cla::TransferOutcome::Completed)
        .await
        .expect("report_transfer should succeed");

    assert_eq!(
        sink.outcomes(),
        vec![(bundle_id, cla::TransferOutcome::Completed)],
        "The BPA should have received the outcome"
    );

    cla.sink.unregister().await;
    server_tasks.shutdown().await;
}

// CLA-CLI-07: the registration declarations round-trip the wire: the whole
// `Request` (address type, lane count, and declared limit) reaches the
// BPA's registrar, and the terms the BPA settles come back in the response
// and reach the CLA's context. Declared and settled limits are distinct
// values, so a relay that echoes the wrong side cannot pass.
#[tokio::test]
async fn cla_cli_07_declarations_round_trip() {
    let lanes = NonZeroU32::new(4).unwrap();
    let declared = NonZeroU64::new(1024 * 1024).unwrap();
    let settled = NonZeroU64::new(512 * 1024).unwrap();

    let bpa = Arc::new(MockBpa::with_max_bundle_size(settled));
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["cla"]).await;

    let cla = RemoteBpa::new(grpc_addr)
        .register_cla(
            "test-cla",
            MockCla::declaring(Request {
                address_type: Some(cla::AddressType::Tcp),
                lane_count: Some(lanes),
                max_bundle_size: Some(declared),
            }),
        )
        .await
        .expect("registration should succeed");

    let request = (*bpa.declared_request.lock()).expect("The BPA should have seen a request");
    assert_eq!(request.address_type, Some(cla::AddressType::Tcp));
    assert_eq!(request.lane_count, Some(lanes));
    assert_eq!(request.max_bundle_size, Some(declared));

    assert_eq!(
        cla.terms,
        Terms {
            lane_count: Some(lanes),
            max_bundle_size: Some(settled),
        },
        "The CLA should receive the terms the BPA settled"
    );

    cla.sink.unregister().await;
    server_tasks.shutdown().await;
}

// CLA-CLI-08: an empty `Request` round-trips as absent in both directions:
// every declaration reaches the BPA as None, and a BPA that settles no
// limit reaches the CLA as None (the same decode an old, pre-field BPA's
// response produces).
#[tokio::test]
async fn cla_cli_08_absent_declarations_round_trip_as_none() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["cla"]).await;

    let cla = RemoteBpa::new(grpc_addr)
        .register_cla("test-cla", MockCla::factory())
        .await
        .expect("registration should succeed");

    let request = (*bpa.declared_request.lock()).expect("The BPA should have seen a request");
    assert_eq!(request.address_type, None);
    assert_eq!(request.lane_count, None);
    assert_eq!(request.max_bundle_size, None);

    assert_eq!(
        cla.terms,
        Terms::default(),
        "The CLA should see no settled terms"
    );

    cla.sink.unregister().await;
    server_tasks.shutdown().await;
}
