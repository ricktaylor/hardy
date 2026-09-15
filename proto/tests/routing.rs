#![cfg(feature = "server")]

mod common;

use std::{net::SocketAddr, sync::Arc};

use hardy_async::TaskPool;
#[cfg(feature = "client")]
use hardy_async::sync::spin::Once;
use hardy_bpa::{Bytes, bpa::Bpa};
#[cfg(feature = "client")]
use hardy_bpa::{
    async_trait,
    routing::{self, RoutingAgent, RoutingSink},
};
#[cfg(feature = "client")]
use hardy_bpv7::{eid::NodeId, status_report::ReasonCode};
#[cfg(feature = "client")]
use hardy_eid_patterns::EidPattern;
#[cfg(feature = "client")]
use hardy_proto::client::BpaClient;
use hardy_proto::{
    routing::{
        AddRouteRequest, AddRouteResponse, Discard, Register, RemoveRouteRequest,
        RemoveRouteResponse, RouteAction, SubscribeRequest, SubscribeResponse, Unregister,
        route_action::Action, routing_service_client::RoutingServiceClient,
        routing_service_server::RoutingServiceServer, subscribe_request, subscribe_response,
    },
    server::RoutingServiceImpl,
};
use tokio::sync::mpsc::{self, Sender};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{
    Code, Status, Streaming,
    transport::{Channel, Server},
};

use common::{UnregisterWatch, build_bpa, ipn1, serve, timeout, wait_unregistered};

struct Harness {
    bpa: Arc<Bpa>,
    #[expect(dead_code, reason = "held for its liveness")]
    tasks: TaskPool,
    client: RoutingServiceClient<Channel>,
    #[cfg_attr(
        not(feature = "client"),
        expect(dead_code, reason = "read by the client SDK test")
    )]
    address: SocketAddr,
    watch: Arc<UnregisterWatch>,
}

async fn harness() -> Harness {
    let bpa = build_bpa(ipn1(), false).await;

    let tasks = TaskPool::new();
    let watch = UnregisterWatch::new(bpa.clone());
    let server = RoutingServiceImpl::new(watch.clone(), tasks.clone());
    let service = RoutingServiceServer::new(server.clone());
    let address = serve(Server::builder().add_service(service)).await;

    let client = RoutingServiceClient::connect(format!("http://{address}"))
        .await
        .unwrap();
    Harness {
        bpa,
        tasks,
        client,
        address,
        watch,
    }
}

struct Registered {
    requests_tx: Sender<SubscribeRequest>,
    events: Streaming<SubscribeResponse>,
    node_ids: Vec<String>,
    token: Bytes,
}

async fn register(client: &mut RoutingServiceClient<Channel>, name: &str) -> Registered {
    let (requests_tx, requests_rx) = mpsc::channel(4);
    requests_tx
        .send(SubscribeRequest {
            request: Some(subscribe_request::Request::Register(Register {
                name: name.to_string(),
            })),
        })
        .await
        .unwrap();

    let mut events = client
        .subscribe(ReceiverStream::new(requests_rx))
        .await
        .unwrap()
        .into_inner();
    let event = timeout(events.message()).await.unwrap().unwrap();
    let Some(subscribe_response::Event::Registration(registration)) = event.event else {
        panic!("expected the Registration event first");
    };
    assert!(!registration.session_token.is_empty());

    Registered {
        requests_tx,
        events,
        node_ids: registration.node_ids,
        token: registration.session_token,
    }
}

fn via(eid: &str) -> RouteAction {
    RouteAction {
        action: Some(Action::Via(eid.to_string())),
    }
}

fn drop_with_reason(reason_code: u64) -> RouteAction {
    RouteAction {
        action: Some(Action::Drop(Discard {
            reason_code: Some(reason_code),
        })),
    }
}

async fn add_route(
    client: &mut RoutingServiceClient<Channel>,
    token: Bytes,
    pattern: &str,
    action: RouteAction,
    priority: u32,
) -> Result<AddRouteResponse, Status> {
    client
        .add_route(AddRouteRequest {
            session_token: token,
            pattern: pattern.to_string(),
            action: Some(action),
            priority,
        })
        .await
        .map(|r| r.into_inner())
}

async fn remove_route(
    client: &mut RoutingServiceClient<Channel>,
    token: Bytes,
    pattern: &str,
    action: RouteAction,
    priority: u32,
) -> Result<RemoveRouteResponse, Status> {
    client
        .remove_route(RemoveRouteRequest {
            session_token: token,
            pattern: pattern.to_string(),
            action: Some(action),
            priority,
        })
        .await
        .map(|r| r.into_inner())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registration_returns_node_ids_and_a_token() {
    let mut harness = harness().await;

    let registered = register(&mut harness.client, "test-agent").await;
    assert_eq!(registered.node_ids, ["ipn:1.0"]);

    let (requests_tx, requests_rx) = mpsc::channel(4);
    requests_tx
        .send(SubscribeRequest {
            request: Some(subscribe_request::Request::Register(Register {
                name: "test-agent".to_string(),
            })),
        })
        .await
        .unwrap();
    let status = timeout(harness.client.subscribe(ReceiverStream::new(requests_rx)))
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::AlreadyExists);

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn routes_are_added_and_removed_once() {
    let mut harness = harness().await;
    let registered = register(&mut harness.client, "test-agent").await;

    assert!(
        add_route(
            &mut harness.client,
            registered.token.clone(),
            "ipn:2.*",
            via("ipn:2.0"),
            100
        )
        .await
        .unwrap()
        .added
    );
    assert!(
        !add_route(
            &mut harness.client,
            registered.token.clone(),
            "ipn:2.*",
            via("ipn:2.0"),
            100
        )
        .await
        .unwrap()
        .added
    );

    assert!(
        remove_route(
            &mut harness.client,
            registered.token.clone(),
            "ipn:2.*",
            via("ipn:2.0"),
            100
        )
        .await
        .unwrap()
        .removed
    );
    assert!(
        !remove_route(
            &mut harness.client,
            registered.token.clone(),
            "ipn:2.*",
            via("ipn:2.0"),
            100
        )
        .await
        .unwrap()
        .removed
    );

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_invalid_pattern_is_rejected() {
    let mut harness = harness().await;
    let registered = register(&mut harness.client, "test-agent").await;

    let status = add_route(
        &mut harness.client,
        registered.token.clone(),
        "not a pattern",
        via("ipn:2.0"),
        100,
    )
    .await
    .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_action_is_rejected() {
    let mut harness = harness().await;
    let registered = register(&mut harness.client, "test-agent").await;

    let status = harness
        .client
        .add_route(AddRouteRequest {
            session_token: registered.token.clone(),
            pattern: "ipn:2.*".to_string(),
            action: None,
            priority: 100,
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reserved_drop_reason_is_rejected() {
    let mut harness = harness().await;
    let registered = register(&mut harness.client, "test-agent").await;

    let status = add_route(
        &mut harness.client,
        registered.token.clone(),
        "ipn:2.*",
        drop_with_reason(255),
        100,
    )
    .await
    .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);

    assert!(
        add_route(
            &mut harness.client,
            registered.token,
            "ipn:2.*",
            drop_with_reason(254),
            100,
        )
        .await
        .unwrap()
        .added
    );

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forged_token_is_rejected() {
    let mut harness = harness().await;
    register(&mut harness.client, "test-agent").await;

    let status = add_route(
        &mut harness.client,
        Bytes::from_static(b"forged"),
        "ipn:2.*",
        via("ipn:2.0"),
        100,
    )
    .await
    .unwrap_err();
    assert_eq!(status.code(), Code::Unauthenticated);

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_stream_tears_the_session_down() {
    let mut harness = harness().await;
    let registered = register(&mut harness.client, "test-agent").await;

    let mut unregistered = harness.watch.subscribe();
    drop(registered.events);
    drop(registered.requests_tx);
    wait_unregistered(&mut unregistered).await;

    let status = add_route(
        &mut harness.client,
        registered.token.clone(),
        "ipn:2.*",
        via("ipn:2.0"),
        100,
    )
    .await
    .unwrap_err();
    assert_eq!(status.code(), Code::Unauthenticated);

    let (requests_tx, requests_rx) = mpsc::channel(4);
    requests_tx
        .send(SubscribeRequest {
            request: Some(subscribe_request::Request::Register(Register {
                name: "test-agent".to_string(),
            })),
        })
        .await
        .unwrap();
    harness
        .client
        .subscribe(ReceiverStream::new(requests_rx))
        .await
        .unwrap();

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unregister_ends_the_session_and_invalidates_the_token() {
    let mut harness = harness().await;
    let mut registered = register(&mut harness.client, "test-agent").await;
    registered
        .requests_tx
        .send(SubscribeRequest {
            request: Some(subscribe_request::Request::Unregister(Unregister {})),
        })
        .await
        .unwrap();
    assert!(
        timeout(registered.events.message())
            .await
            .unwrap()
            .is_none(),
        "unregister must end the session stream"
    );

    let status = add_route(
        &mut harness.client,
        registered.token.clone(),
        "ipn:2.*",
        via("ipn:2.0"),
        100,
    )
    .await
    .unwrap_err();
    assert_eq!(status.code(), Code::Unauthenticated);

    harness.bpa.shutdown().await;
}

#[cfg(feature = "client")]
struct SdkAgent {
    sink: Once<Box<dyn RoutingSink>>,
}

#[cfg(feature = "client")]
#[async_trait]
impl RoutingAgent for SdkAgent {
    async fn on_register(&self, sink: Box<dyn RoutingSink>, _node_ids: &[NodeId]) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}
}

#[cfg(feature = "client")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_sdk_roundtrip() {
    let harness = harness().await;
    let client = BpaClient::new(format!("http://{}", harness.address), TaskPool::new()).unwrap();

    let agent = Arc::new(SdkAgent { sink: Once::new() });
    let handle = client
        .register_routing_agent("sdk-agent".to_string(), agent.clone())
        .await
        .unwrap();
    assert_eq!(handle.id().len(), 1);

    let sink = agent.sink.get().unwrap();
    let pattern: EidPattern = "ipn:2.*".parse().unwrap();
    let action = routing::RouteAction::Via("ipn:2.0".parse().unwrap());

    assert!(
        sink.add_route(pattern.clone(), action.clone(), 50)
            .await
            .unwrap()
    );
    assert!(
        !sink
            .add_route(pattern.clone(), action.clone(), 50)
            .await
            .unwrap()
    );
    assert!(sink.remove_route(&pattern, &action, 50).await.unwrap());
    assert!(!sink.remove_route(&pattern, &action, 50).await.unwrap());

    let reserved = routing::RouteAction::Drop(Some(ReasonCode::Unassigned(255)));
    assert!(matches!(
        sink.add_route(pattern.clone(), reserved, 60).await,
        Err(routing::Error::Internal(_))
    ));

    sink.unregister().await;
    harness.bpa.shutdown().await;
}
