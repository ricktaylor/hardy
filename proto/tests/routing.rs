//! Routing Agent client proxy tests (RTE-CLI-01 through RTE-CLI-04).
//!
//! The routing agent client maps its Rust trait calls onto routing.proto
//! messages, across the gRPC proxy.

mod common;

use common::MockBpa;
use hardy_bpa::node_ids::NodeIds;
use hardy_bpa::routing::{
    Agent, Context, Error, Factory, RegistrarExt, RouteAction, Sink, factory_fn,
};
use hardy_bpv7::eid::NodeId;
use hardy_proto::client::RemoteBpa;
use std::sync::Arc;

// A mock routing agent that records the context it was built from.
struct MockRoutingAgent {
    sink: Arc<dyn Sink>,
    node_ids: Arc<NodeIds>,
}

impl MockRoutingAgent {
    fn factory() -> impl Factory<Component = Self> {
        factory_fn(|cx: Context| {
            Ok(Arc::new(Self {
                sink: cx.sink,
                node_ids: cx.node_ids,
            }))
        })
    }
}

impl Agent for MockRoutingAgent {}

// RTE-CLI-01: Register routing agent, receive node IDs.
//
// The client registers a routing agent via RemoteBpa. The mock BPA
// builds it from a context carrying a sink and its node IDs, both of
// which cross the wire to the agent.
#[tokio::test]
async fn rte_cli_01_registration() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["routing"]).await;

    let agent = RemoteBpa::new(grpc_addr)
        .register_routing_agent("test-agent", MockRoutingAgent::factory())
        .await
        .expect("registration should succeed");

    assert!(
        !Vec::<NodeId>::from(agent.node_ids.as_ref()).is_empty(),
        "agent should receive at least one node ID"
    );

    agent.sink.unregister().await;
    server_tasks.shutdown().await;
}

// RTE-CLI-02: Add route via sink.
//
// After registration, the agent uses its sink to add a route. The
// request goes through the gRPC proxy to the mock BPA's routing sink,
// which returns success.
#[tokio::test]
async fn rte_cli_02_add_route() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["routing"]).await;

    let agent = RemoteBpa::new(grpc_addr)
        .register_routing_agent("test-agent", MockRoutingAgent::factory())
        .await
        .expect("registration should succeed");

    let pattern = "ipn:2.*.*".parse().expect("valid pattern");
    let action = RouteAction::Via("ipn:2.1.0".parse().expect("valid EID"));
    let added = agent
        .sink
        .add_route(pattern, action, 100)
        .await
        .expect("add_route should succeed");

    assert!(added, "route should be newly added");

    agent.sink.unregister().await;
    server_tasks.shutdown().await;
}

// RTE-CLI-03: Remove route via sink.
//
// After registration, the agent uses its sink to remove a route. The
// request goes through the gRPC proxy to the mock BPA's routing sink,
// which returns success.
#[tokio::test]
async fn rte_cli_03_remove_route() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["routing"]).await;

    let agent = RemoteBpa::new(grpc_addr)
        .register_routing_agent("test-agent", MockRoutingAgent::factory())
        .await
        .expect("registration should succeed");

    let pattern = "ipn:2.*.*".parse().expect("valid pattern");
    let action = RouteAction::Via("ipn:2.1.0".parse().expect("valid EID"));
    let removed = agent
        .sink
        .remove_route(&pattern, &action, 100)
        .await
        .expect("remove_route should succeed");

    assert!(removed, "route should be removed");

    agent.sink.unregister().await;
    server_tasks.shutdown().await;
}

// RTE-CLI-04: A refused registration reports why it was refused.
//
// The BPA rejects the second agent to claim a name. The refusal travels
// as the registration stream's own status, so the client learns the
// reason rather than watching the stream disappear.
#[tokio::test]
async fn rte_cli_04_refused_registration_reports_its_status() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["routing"]).await;
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let agent = remote_bpa
        .register_routing_agent("test-agent", MockRoutingAgent::factory())
        .await
        .expect("the first registration should succeed");

    let Err(error) = remote_bpa
        .register_routing_agent("test-agent", MockRoutingAgent::factory())
        .await
    else {
        panic!("a name already taken should be refused");
    };

    let Error::Internal(source) = error else {
        panic!("the refusal should carry the BPA's status, got {error}");
    };
    let status = source
        .downcast_ref::<tonic::Status>()
        .expect("the refusal should carry the BPA's status");
    assert_eq!(status.code(), tonic::Code::AlreadyExists);

    agent.sink.unregister().await;
    server_tasks.shutdown().await;
}
