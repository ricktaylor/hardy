//! Lifecycle tests for gRPC proxy unregistration (LIFE-01 through LIFE-06).
//!
//! These tests validate that stream close correctly triggers cleanup on
//! both client and server sides for all shutdown scenarios.

mod common;

use common::MockBpa;
use hardy_bpa::async_trait;
use hardy_bpa::bpa::BpaRegistration;
use hardy_bpa::routes::{RoutingAgent, RoutingContext};
use hardy_bpv7::eid::NodeId;
use hardy_proto::client::RemoteBpa;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct MockRoutingAgent {
    registered: AtomicBool,
    unregister_count: AtomicUsize,
    ctx: hardy_async::sync::spin::Mutex<Option<RoutingContext>>,
}

impl MockRoutingAgent {
    fn new() -> Self {
        Self {
            registered: AtomicBool::new(false),
            unregister_count: AtomicUsize::new(0),
            ctx: hardy_async::sync::spin::Mutex::new(None),
        }
    }

    fn is_registered(&self) -> bool {
        self.registered.load(Ordering::Relaxed)
    }

    fn is_unregistered(&self) -> bool {
        self.unregister_count.load(Ordering::Relaxed) > 0
    }

    fn unregister_count(&self) -> usize {
        self.unregister_count.load(Ordering::Relaxed)
    }

    fn take_ctx(&self) -> Option<RoutingContext> {
        self.ctx.lock().take()
    }
}

#[async_trait]
impl RoutingAgent for MockRoutingAgent {
    async fn on_register(&self, ctx: RoutingContext, _node_ids: &[NodeId]) {
        *self.ctx.lock() = Some(ctx);
        self.registered.store(true, Ordering::Relaxed);
    }

    async fn on_unregister(&self) {
        self.unregister_count.fetch_add(1, Ordering::Relaxed);
    }
}

// LIFE-01: Client-initiated disconnect via dropping context.
//
// The client drops the RoutingContext, closing the channels.
// The server detects the close, unregisters the component.
// The client receives a synthetic `on_unregister()` callback.
#[tokio::test]
async fn life_01_client_initiated_unregister() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["routing"]).await;

    let agent = Arc::new(MockRoutingAgent::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let node_ids: Vec<NodeId> = remote_bpa
        .register_routing_agent("test-agent".to_string(), agent.clone())
        .await
        .expect("registration should succeed");

    assert!(!node_ids.is_empty(), "should receive node IDs");
    assert!(agent.is_registered());
    assert!(!agent.is_unregistered());

    // Client-initiated disconnect: drop the context
    drop(agent.take_ctx());

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    assert!(
        agent.is_unregistered(),
        "agent should have received on_unregister via on_close"
    );

    server_tasks.shutdown().await;
}

// LIFE-02: BPA-initiated unregister.
//
// The BPA calls `on_unregister()` on the server-side RemoteRoutingAgent
// (simulating BPA shutdown). The server shuts down the proxy, closing the
// stream. The client receives a synthetic `on_unregister()` via `on_close`.
#[tokio::test]
async fn life_02_bpa_initiated_unregister() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["routing"]).await;

    let agent = Arc::new(MockRoutingAgent::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _node_ids: Vec<NodeId> = remote_bpa
        .register_routing_agent("test-agent".to_string(), agent.clone())
        .await
        .expect("registration should succeed");

    assert!(agent.is_registered());
    assert!(!agent.is_unregistered());

    let server_agent = bpa
        .last_routing_agent
        .lock()
        .clone()
        .expect("BPA should have the server-side agent");
    server_agent.on_unregister().await;

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    assert!(
        agent.is_unregistered(),
        "agent should have received on_unregister via on_close"
    );

    server_tasks.shutdown().await;
}

// LIFE-03: Client drops context without explicit disconnect.
//
// The client drops its context. The channel closes, the server detects it
// via the proxy's on_close, and disconnects the component.
#[tokio::test]
async fn life_03_drop_without_unregister() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["routing"]).await;

    let agent = Arc::new(MockRoutingAgent::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _node_ids: Vec<NodeId> = remote_bpa
        .register_routing_agent("test-agent".to_string(), agent.clone())
        .await
        .expect("registration should succeed");

    assert!(agent.is_registered());

    // Drop the context without explicit disconnect
    drop(agent.take_ctx());

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // The server should have detected the close and cleaned up
    assert!(
        agent.is_unregistered(),
        "agent should have received on_unregister after context drop"
    );

    server_tasks.shutdown().await;
}

// LIFE-04: Server crashes while client is connected.
#[tokio::test]
async fn life_04_server_crash() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["routing"]).await;

    let agent = Arc::new(MockRoutingAgent::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _node_ids: Vec<NodeId> = remote_bpa
        .register_routing_agent("test-agent".to_string(), agent.clone())
        .await
        .expect("registration should succeed");

    assert!(agent.is_registered());
    assert!(!agent.is_unregistered());

    bpa.crash().await;

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    assert!(
        agent.is_unregistered(),
        "agent should have received on_unregister via on_close"
    );

    server_tasks.shutdown().await;
}

// LIFE-05: Client and BPA disconnect simultaneously.
#[tokio::test]
async fn life_05_simultaneous_unregister() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["routing"]).await;

    let agent = Arc::new(MockRoutingAgent::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _node_ids: Vec<NodeId> = remote_bpa
        .register_routing_agent("test-agent".to_string(), agent.clone())
        .await
        .expect("registration should succeed");

    assert!(agent.is_registered());

    // Fire both disconnect paths concurrently
    let ctx = agent.take_ctx().expect("agent should have a context");
    let bpa_clone = bpa.clone();
    let (_, _) = tokio::join!(async { drop(ctx) }, bpa_clone.crash());

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    assert!(
        agent.is_unregistered(),
        "agent should have received on_unregister"
    );

    server_tasks.shutdown().await;
}

// LIFE-06: Client receives on_unregister exactly once.
#[tokio::test]
async fn life_06_exactly_once_unregister() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["routing"]).await;

    let agent = Arc::new(MockRoutingAgent::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _node_ids: Vec<NodeId> = remote_bpa
        .register_routing_agent("test-agent".to_string(), agent.clone())
        .await
        .expect("registration should succeed");

    assert_eq!(agent.unregister_count(), 0);

    bpa.crash().await;

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    assert_eq!(
        agent.unregister_count(),
        1,
        "on_unregister should be called exactly once"
    );

    server_tasks.shutdown().await;
}
