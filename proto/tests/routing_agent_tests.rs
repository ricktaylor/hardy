//! Routing Agent client proxy tests (RTE-CLI-01 through RTE-CLI-03).
//!
//! Verify the routing agent client correctly maps Rust trait calls
//! to routing.proto messages via the gRPC proxy.

mod common;

use common::MockBpa;
use hardy_bpa::async_trait;
use hardy_bpa::bpa::BpaRegistration;
use hardy_bpa::routes::{Action, RoutingAgent, RoutingContext};
use hardy_bpv7::eid::NodeId;
use hardy_proto::client::RemoteBpa;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

struct MockRoutingAgent {
    registered: AtomicBool,
    ctx: hardy_async::sync::spin::Mutex<Option<RoutingContext>>,
}

impl MockRoutingAgent {
    fn new() -> Self {
        Self {
            registered: AtomicBool::new(false),
            ctx: hardy_async::sync::spin::Mutex::new(None),
        }
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

    async fn on_unregister(&self) {}
}

// RTE-CLI-01: Register routing agent, receive node IDs.
#[tokio::test]
async fn rte_cli_01_registration() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["routing"]).await;

    let agent = Arc::new(MockRoutingAgent::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let node_ids: Vec<NodeId> = remote_bpa
        .register_routing_agent("test-agent".to_string(), agent.clone())
        .await
        .expect("registration should succeed");

    assert!(!node_ids.is_empty(), "should receive at least one node ID");
    assert!(
        agent.registered.load(Ordering::Relaxed),
        "agent should have received on_register"
    );
    assert!(
        agent.ctx.lock().is_some(),
        "agent should have a context after registration"
    );

    drop(agent.take_ctx());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    server_tasks.shutdown().await;
}

// RTE-CLI-02: Add route via context.
//
// After registration, the agent uses its context to add a route. The
// request goes through the gRPC proxy to the mock BPA's channel receiver.
#[tokio::test]
async fn rte_cli_02_add_route() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["routing"]).await;

    let agent = Arc::new(MockRoutingAgent::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _node_ids: Vec<NodeId> = remote_bpa
        .register_routing_agent("test-agent".to_string(), agent.clone())
        .await
        .expect("registration should succeed");

    let ctx = agent.take_ctx().expect("agent should have a context");

    let pattern = "ipn:2.*.*".parse().expect("valid pattern");
    let action = Action::Via("ipn:2.1.0".parse().expect("valid EID"));
    ctx.add_route(pattern, action, 100);

    // Give the channel time to process
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    drop(ctx);
    server_tasks.shutdown().await;
}

// RTE-CLI-03: Remove route via context.
#[tokio::test]
async fn rte_cli_03_remove_route() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["routing"]).await;

    let agent = Arc::new(MockRoutingAgent::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _node_ids: Vec<NodeId> = remote_bpa
        .register_routing_agent("test-agent".to_string(), agent.clone())
        .await
        .expect("registration should succeed");

    let ctx = agent.take_ctx().expect("agent should have a context");

    let pattern = "ipn:2.*.*".parse().expect("valid pattern");
    let action = Action::Via("ipn:2.1.0".parse().expect("valid EID"));
    ctx.remove_route(&pattern, &action, 100);

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    drop(ctx);
    server_tasks.shutdown().await;
}
