//! Routing Agent client proxy tests (RTE-CLI-01 through RTE-CLI-03).
//!
//! Verify the routing agent client correctly maps Rust trait calls
//! to routing.proto messages via the gRPC proxy.

mod common;

use common::MockBpa;
use hardy_bpa::async_trait;
use hardy_bpa::bpa::BpaRegistration;
use hardy_bpa::routes::{Action, RoutingAgent, RoutingSink};
use hardy_bpv7::eid::NodeId;
use hardy_proto::client::RemoteBpa;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A mock RoutingAgent that stores the sink for test use.
struct MockRoutingAgent {
    registered: AtomicBool,
    sink: hardy_async::sync::spin::Mutex<Option<Box<dyn RoutingSink>>>,
}

impl MockRoutingAgent {
    fn new() -> Self {
        Self {
            registered: AtomicBool::new(false),
            sink: hardy_async::sync::spin::Mutex::new(None),
        }
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

    async fn on_unregister(&self) {}
}

/// RTE-CLI-01: Register routing agent, receive node IDs.
///
/// The client registers a routing agent via RemoteBpa. The mock BPA
/// calls on_register with a sink and node IDs. The client receives
/// the node IDs and the agent receives the sink.
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
        agent.sink.lock().is_some(),
        "agent should have a sink after registration"
    );

    // Clean up
    drop(agent.take_sink());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    server_tasks.shutdown().await;
}

/// RTE-CLI-02: Add route via sink.
///
/// After registration, the agent uses its sink to add a route. The
/// request goes through the gRPC proxy to the mock BPA's RoutingSink,
/// which returns success.
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

    let sink = agent.take_sink().expect("agent should have a sink");

    let pattern = "ipn:2.*.*".parse().expect("valid pattern");
    let action = Action::Via("ipn:2.1.0".parse().expect("valid EID"));
    let added = sink
        .add_route(pattern, action, 100)
        .await
        .expect("add_route should succeed");

    assert!(added, "route should be newly added");

    // Clean up
    sink.unregister().await;
    server_tasks.shutdown().await;
}

/// RTE-CLI-03: Remove route via sink.
///
/// After registration, the agent uses its sink to remove a route. The
/// request goes through the gRPC proxy to the mock BPA's RoutingSink,
/// which returns success.
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

    let sink = agent.take_sink().expect("agent should have a sink");

    let pattern = "ipn:2.*.*".parse().expect("valid pattern");
    let action = Action::Via("ipn:2.1.0".parse().expect("valid EID"));
    let removed = sink
        .remove_route(&pattern, &action, 100)
        .await
        .expect("remove_route should succeed");

    assert!(removed, "route should be removed");

    // Clean up
    sink.unregister().await;
    server_tasks.shutdown().await;
}
