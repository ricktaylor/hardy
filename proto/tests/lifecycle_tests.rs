//! Lifecycle tests for gRPC proxy unregistration (LIFE-01 through LIFE-06).
//!
//! These tests validate that stream close correctly triggers cleanup on
//! both client and server sides for all shutdown scenarios.

mod common;

use common::MockBpa;
use hardy_bpa::async_trait;
use hardy_bpa::bpa::BpaRegistration;
use hardy_bpa::routes::{RoutingAgent, RoutingSink};
use hardy_bpv7::eid::NodeId;
use hardy_proto::client::RemoteBpa;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// A mock RoutingAgent that records lifecycle callbacks.
struct MockRoutingAgent {
    registered: AtomicBool,
    unregister_count: AtomicUsize,
    sink: hardy_async::sync::spin::Mutex<Option<Box<dyn RoutingSink>>>,
}

impl MockRoutingAgent {
    fn new() -> Self {
        Self {
            registered: AtomicBool::new(false),
            unregister_count: AtomicUsize::new(0),
            sink: hardy_async::sync::spin::Mutex::new(None),
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

    fn take_sink(&self) -> Option<Box<dyn RoutingSink>> {
        self.sink.lock().take()
    }
}

#[async_trait]
impl RoutingAgent for MockRoutingAgent {
    async fn on_register(&self, sink: Box<dyn RoutingSink>, _node_ids: &[NodeId]) {
        *self.sink.lock() = Some(sink);
        self.registered.store(true, Ordering::Relaxed);
    }

    async fn on_unregister(&self) {
        self.unregister_count.fetch_add(1, Ordering::Relaxed);
    }
}

/// LIFE-01: Client-initiated unregister via stream close.
///
/// The client calls `Sink::unregister()` which shuts down the proxy,
/// closing the stream. The server detects the close via `on_close`,
/// unregisters the component from the mock BPA, and cancels the proxy.
/// The client receives a synthetic `on_unregister()` callback.
#[tokio::test]
async fn life_01_client_initiated_unregister() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_routing_server(&bpa).await;

    // Create a mock routing agent and register it via the gRPC client
    let agent = Arc::new(MockRoutingAgent::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let node_ids: Vec<NodeId> = remote_bpa
        .register_routing_agent("test-agent".to_string(), agent.clone())
        .await
        .expect("registration should succeed");

    assert!(!node_ids.is_empty(), "should receive node IDs");
    assert!(
        agent.is_registered(),
        "agent should have received on_register"
    );
    assert!(
        !agent.is_unregistered(),
        "agent should not be unregistered yet"
    );

    // The mock BPA should have received the registration
    assert!(
        bpa.last_routing_sink.lock().is_some(),
        "BPA should have a routing sink"
    );

    // Client-initiated unregister: take the sink and call unregister()
    let sink = agent
        .take_sink()
        .expect("agent should have a sink from on_register");
    sink.unregister().await;

    // Give the server a moment to process the stream close
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // The client should have received a synthetic on_unregister()
    assert!(
        agent.is_unregistered(),
        "agent should have received on_unregister via on_close"
    );

    // Clean up server
    server_tasks.shutdown().await;
}

/// LIFE-02: BPA-initiated unregister.
///
/// The BPA calls `on_unregister()` on the server-side RemoteRoutingAgent
/// (simulating BPA shutdown). The server shuts down the proxy, closing the
/// stream. The client receives a synthetic `on_unregister()` via `on_close`.
#[tokio::test]
async fn life_02_bpa_initiated_unregister() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_routing_server(&bpa).await;

    let agent = Arc::new(MockRoutingAgent::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _node_ids: Vec<NodeId> = remote_bpa
        .register_routing_agent("test-agent".to_string(), agent.clone())
        .await
        .expect("registration should succeed");

    assert!(agent.is_registered());
    assert!(!agent.is_unregistered());

    // BPA-initiated: call on_unregister on the server-side RemoteRoutingAgent
    let server_agent = bpa
        .last_routing_agent
        .lock()
        .clone()
        .expect("BPA should have the server-side agent");
    server_agent.on_unregister().await;

    // Give the client a moment to detect the stream close
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // The client should have received a synthetic on_unregister()
    assert!(
        agent.is_unregistered(),
        "agent should have received on_unregister via on_close"
    );

    // Clean up server
    server_tasks.shutdown().await;
}

/// LIFE-03: Client drops proxy without calling unregister.
///
/// The client drops its sink (and thus the proxy) without calling
/// `unregister()`. The proxy's `Drop` impl cancels the tasks, closing
/// the stream. The server detects the close via `on_close`, unregisters
/// the component from the BPA, and cancels the server-side proxy.
#[tokio::test]
async fn life_03_drop_without_unregister() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_routing_server(&bpa).await;

    let agent = Arc::new(MockRoutingAgent::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _node_ids: Vec<NodeId> = remote_bpa
        .register_routing_agent("test-agent".to_string(), agent.clone())
        .await
        .expect("registration should succeed");

    assert!(agent.is_registered());

    // Verify the mock BPA received the registration
    let sink = bpa
        .last_routing_sink
        .lock()
        .clone()
        .expect("BPA should have a routing sink");
    assert!(!sink.is_unregistered());

    // Drop the sink without calling unregister.
    drop(agent.take_sink());

    // Give the server a moment to detect the stream close and clean up
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // The server's on_close should have called sink.unregister() on the BPA
    assert!(
        sink.is_unregistered(),
        "BPA sink should have been unregistered by server on_close"
    );

    // Clean up server
    server_tasks.shutdown().await;
}

/// LIFE-04: Server crashes while client is connected.
///
/// The server-side BPA forcefully unregisters all agents (simulating a
/// crash or abrupt shutdown). The client detects the stream close and
/// delivers a synthetic `on_unregister()` to the trait impl via `on_close`.
#[tokio::test]
async fn life_04_server_crash() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_routing_server(&bpa).await;

    let agent = Arc::new(MockRoutingAgent::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _node_ids: Vec<NodeId> = remote_bpa
        .register_routing_agent("test-agent".to_string(), agent.clone())
        .await
        .expect("registration should succeed");

    assert!(agent.is_registered());
    assert!(!agent.is_unregistered());

    // Simulate crash: force-unregister all agents
    bpa.crash().await;

    // Give the client a moment to detect the stream close
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // The client should have received a synthetic on_unregister()
    assert!(
        agent.is_unregistered(),
        "agent should have received on_unregister via on_close"
    );

    // Clean up server
    server_tasks.shutdown().await;
}

/// LIFE-05: Client and BPA unregister simultaneously.
///
/// Both the client and BPA initiate unregister concurrently. The
/// `Mutex<Option>.take()` on the server ensures exactly one path
/// takes the sink. No double-unregister, no deadlock.
#[tokio::test]
async fn life_05_simultaneous_unregister() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_routing_server(&bpa).await;

    let agent = Arc::new(MockRoutingAgent::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _node_ids: Vec<NodeId> = remote_bpa
        .register_routing_agent("test-agent".to_string(), agent.clone())
        .await
        .expect("registration should succeed");

    assert!(agent.is_registered());

    // Fire both unregister paths concurrently
    let sink = agent.take_sink().expect("agent should have a sink");
    let bpa_clone = bpa.clone();
    let (_, _) = tokio::join!(sink.unregister(), bpa_clone.crash());

    // Give everything a moment to settle
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // The client should have received on_unregister
    assert!(
        agent.is_unregistered(),
        "agent should have received on_unregister"
    );

    // No panic, no deadlock — test completing is the assertion
    server_tasks.shutdown().await;
}

/// LIFE-06: Client receives on_unregister exactly once.
///
/// After BPA-initiated unregister (which closes the stream), the client
/// must receive exactly one `on_unregister()` call — not zero, not two.
#[tokio::test]
async fn life_06_exactly_once_unregister() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_routing_server(&bpa).await;

    let agent = Arc::new(MockRoutingAgent::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _node_ids: Vec<NodeId> = remote_bpa
        .register_routing_agent("test-agent".to_string(), agent.clone())
        .await
        .expect("registration should succeed");

    assert_eq!(agent.unregister_count(), 0);

    // BPA-initiated unregister
    bpa.crash().await;

    // Give the client time to process
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Exactly one on_unregister — not zero (missed), not two (duplicate)
    assert_eq!(
        agent.unregister_count(),
        1,
        "on_unregister should be called exactly once"
    );

    server_tasks.shutdown().await;
}
