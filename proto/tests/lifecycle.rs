//! Lifecycle tests for gRPC proxy unregistration (LIFE-01 through LIFE-06).
//!
//! These tests validate that stream close correctly triggers cleanup on
//! both client and server sides for all shutdown scenarios.

mod common;

use common::MockBpa;
use hardy_bpa::async_trait;
use hardy_bpa::routing::{Agent, Context, Factory, RegistrarExt, Sink, factory_fn};
use hardy_proto::client::RemoteBpa;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

// A mock routing agent that counts the unregistrations the session drives.
struct MockRoutingAgent {
    sink: Arc<dyn Sink>,
    unregisters: AtomicUsize,
}

impl MockRoutingAgent {
    fn factory() -> impl Factory<Component = Self> {
        factory_fn(|cx: Context| {
            Ok(Arc::new(Self {
                sink: cx.sink,
                unregisters: AtomicUsize::new(0),
            }))
        })
    }

    fn is_unregistered(&self) -> bool {
        self.unregister_count() > 0
    }

    fn unregister_count(&self) -> usize {
        self.unregisters.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl Agent for MockRoutingAgent {
    async fn unregister(&self) -> hardy_bpa::routing::Result<()> {
        self.unregisters.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

// LIFE-01: Client-initiated unregister via stream close.
//
// The client calls `Sink::unregister()` which shuts down the proxy,
// closing the stream. The server detects the close via `on_close`,
// unregisters the agent from the mock BPA, and cancels the proxy. The
// client shuts its own agent down.
#[tokio::test]
async fn life_01_client_initiated_unregister() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["routing"]).await;

    let agent = RemoteBpa::new(grpc_addr)
        .register_routing_agent("test-agent", MockRoutingAgent::factory())
        .await
        .expect("registration should succeed");

    assert!(
        !agent.is_unregistered(),
        "agent should not be unregistered yet"
    );

    // The mock BPA should have received the registration
    let bpa_sink = bpa
        .last_routing_sink
        .lock()
        .clone()
        .expect("BPA should have a routing sink");
    assert!(!bpa_sink.is_unregistered());

    // Client-initiated unregister
    agent.sink.unregister().await;

    // Give the server a moment to process the stream close
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    assert!(
        agent.is_unregistered(),
        "agent should have been unregistered via on_close"
    );
    assert!(
        bpa_sink.is_unregistered(),
        "BPA sink should have been unregistered by the server's on_close"
    );

    // Clean up server
    server_tasks.shutdown().await;
}

// LIFE-02: BPA-initiated unregister.
//
// The BPA unregisters the server-side agent (simulating BPA shutdown).
// The server shuts down the proxy, closing the stream, and the client
// shuts its own agent down via `on_close`.
#[tokio::test]
async fn life_02_bpa_initiated_unregister() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["routing"]).await;

    let agent = RemoteBpa::new(grpc_addr)
        .register_routing_agent("test-agent", MockRoutingAgent::factory())
        .await
        .expect("registration should succeed");

    assert!(!agent.is_unregistered());

    // BPA-initiated: shut down the server-side agent
    let server_agent = bpa
        .last_routing_agent
        .lock()
        .clone()
        .expect("BPA should have the server-side agent");
    server_agent
        .unregister()
        .await
        .expect("the agent unregisters cleanly");

    // Give the client a moment to detect the stream close
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    assert!(
        agent.is_unregistered(),
        "agent should have been unregistered via on_close"
    );

    // Clean up server
    server_tasks.shutdown().await;
}

// LIFE-04: Server crashes while client is connected.
//
// The server-side BPA forcefully shuts down every component (simulating
// a crash or abrupt shutdown). The client detects the stream close and
// shuts its own agent down via `on_close`.
#[tokio::test]
async fn life_04_server_crash() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["routing"]).await;

    let agent = RemoteBpa::new(grpc_addr)
        .register_routing_agent("test-agent", MockRoutingAgent::factory())
        .await
        .expect("registration should succeed");

    assert!(!agent.is_unregistered());

    // Simulate crash: force-unregister every registered component
    bpa.crash().await;

    // Give the client a moment to detect the stream close
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    assert!(
        agent.is_unregistered(),
        "agent should have been unregistered via on_close"
    );

    // Clean up server
    server_tasks.shutdown().await;
}

// LIFE-05: Client and BPA unregister simultaneously.
//
// Both ends tear the session down at once. Each end takes its agent out
// of the session exactly once, so neither a double unregistration nor a
// deadlock can follow.
#[tokio::test]
async fn life_05_simultaneous_unregister() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["routing"]).await;

    let agent = RemoteBpa::new(grpc_addr)
        .register_routing_agent("test-agent", MockRoutingAgent::factory())
        .await
        .expect("registration should succeed");

    // Fire both unregister paths concurrently
    let bpa_clone = bpa.clone();
    let (_, _) = tokio::join!(agent.sink.unregister(), bpa_clone.crash());

    // Give everything a moment to settle
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    assert_eq!(
        agent.unregister_count(),
        1,
        "agent should have been unregistered exactly once"
    );

    // No panic, no deadlock — test completing is the assertion
    server_tasks.shutdown().await;
}

// LIFE-06: Client unregisters its agent exactly once.
//
// After BPA-initiated unregister (which closes the stream), the client
// must unregister its agent exactly once — not zero times, not twice.
#[tokio::test]
async fn life_06_exactly_once_unregister() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["routing"]).await;

    let agent = RemoteBpa::new(grpc_addr)
        .register_routing_agent("test-agent", MockRoutingAgent::factory())
        .await
        .expect("registration should succeed");

    assert_eq!(agent.unregister_count(), 0);

    // BPA-initiated unregister
    bpa.crash().await;

    // Give the client time to process
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Exactly one unregistration — not zero (missed), not two (duplicate)
    assert_eq!(
        agent.unregister_count(),
        1,
        "the agent should be unregistered exactly once"
    );

    server_tasks.shutdown().await;
}
