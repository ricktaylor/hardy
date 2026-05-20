//! CLA client proxy tests (CLA-CLI-01 through CLA-CLI-05).
//!
//! Verify the CLA client correctly maps Rust trait calls to cla.proto
//! messages via the gRPC proxy.

mod common;

use common::MockBpa;
use hardy_bpa::async_trait;
use hardy_bpa::bpa::BpaRegistration;
use hardy_bpa::cla::{self, ClaAddress, ClaContext, ForwardBundleResult};
use hardy_bpv7::eid::NodeId;
use hardy_proto::client::RemoteBpa;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

struct MockCla {
    registered: AtomicBool,
    ctx: hardy_async::sync::spin::Mutex<Option<ClaContext>>,
    forwarded: AtomicBool,
}

impl MockCla {
    fn new() -> Self {
        Self {
            registered: AtomicBool::new(false),
            ctx: hardy_async::sync::spin::Mutex::new(None),
            forwarded: AtomicBool::new(false),
        }
    }

    fn take_ctx(&self) -> Option<ClaContext> {
        self.ctx.lock().take()
    }

    fn is_forwarded(&self) -> bool {
        self.forwarded.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl cla::Cla for MockCla {
    async fn on_register(&self, ctx: ClaContext, _node_ids: &[NodeId]) {
        *self.ctx.lock() = Some(ctx);
        self.registered.store(true, Ordering::Relaxed);
    }

    async fn on_unregister(&self) {}

    async fn forward(
        &self,
        _queue: Option<u32>,
        _cla_addr: &ClaAddress,
        _bundle: hardy_bpa::Bytes,
    ) -> cla::Result<ForwardBundleResult> {
        self.forwarded.store(true, Ordering::Relaxed);
        Ok(ForwardBundleResult::Sent)
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
        .register_cla("test-cla".to_string(), cla.clone(), None)
        .await
        .expect("registration should succeed");

    assert!(!node_ids.is_empty(), "should receive at least one node ID");
    assert!(
        cla.registered.load(Ordering::Relaxed),
        "CLA should have received on_register"
    );
    assert!(
        cla.ctx.lock().is_some(),
        "CLA should have a context after registration"
    );

    drop(cla.take_ctx());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
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
        .register_cla("test-cla".to_string(), cla.clone(), None)
        .await
        .expect("registration should succeed");

    let ctx = cla.take_ctx().expect("CLA should have a context");

    let bundle_data = hardy_bpa::Bytes::from_static(b"\x9f\x89\x07\x00\x00\x82\x01\x00");
    ctx.dispatch(bundle_data, None, None);

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    drop(ctx);
    server_tasks.shutdown().await;
}

// CLA-CLI-03: Forward bundle from BPA to CLA.
#[tokio::test]
async fn cla_cli_03_forward_bundle() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["cla"]).await;

    let cla = Arc::new(MockCla::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _node_ids: Vec<NodeId> = remote_bpa
        .register_cla("test-cla".to_string(), cla.clone(), None)
        .await
        .expect("registration should succeed");

    assert!(!cla.is_forwarded());

    let server_cla = bpa
        .last_cla
        .lock()
        .clone()
        .expect("BPA should have the server-side CLA");

    let addr = ClaAddress::Tcp("127.0.0.1:4556".parse().unwrap());
    let bundle = hardy_bpa::Bytes::from_static(b"\x9f\x89\x07\x00\x00");
    let result = server_cla
        .forward(None, &addr, bundle)
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

    drop(cla.take_ctx());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
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
        .register_cla("test-cla".to_string(), cla.clone(), None)
        .await
        .expect("registration should succeed");

    let ctx = cla.take_ctx().expect("CLA should have a context");

    let addr = ClaAddress::Tcp("192.168.1.1:4556".parse().unwrap());
    let peer_node: NodeId = "ipn:2.0".parse().unwrap();
    ctx.add_peer(addr, vec![peer_node]);

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    drop(ctx);
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
        .register_cla("test-cla".to_string(), cla.clone(), None)
        .await
        .expect("registration should succeed");

    let ctx = cla.take_ctx().expect("CLA should have a context");

    let addr = ClaAddress::Tcp("192.168.1.1:4556".parse().unwrap());
    ctx.remove_peer(addr);

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    drop(ctx);
    server_tasks.shutdown().await;
}
