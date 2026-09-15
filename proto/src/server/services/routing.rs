// The routing surface: the `hardy.routing.v1` wire served against the
// routing surface of a BPA. The template minus the data plane: routing
// agents are push-only, so the session carries only the Registration
// event and then anchors liveness, and the two token-gated doors drive
// the RIB directly.

use core::ops::ControlFlow;
use std::sync::Arc;

use hardy_async::{CancellationToken, TaskPool};
use hardy_bpa::{
    async_trait,
    bpa::BpaRegistration,
    routing::{self, Error, RoutingAgent, RoutingSink},
};
use hardy_bpv7::eid::NodeId;
use hardy_eid_patterns::EidPattern;
use tokio::sync::mpsc;
use tonic::{Request, Response, Status, Streaming};
#[cfg(feature = "instrument")]
use tracing::instrument;
use tracing::{error, warn};

use crate::{
    routing::{
        AddRouteRequest, AddRouteResponse, Registration, RemoveRouteRequest, RemoveRouteResponse,
        SubscribeRequest, SubscribeResponse, routing_agent_service_server::RoutingAgentService,
        subscribe_request, subscribe_response,
    },
    server::{
        session::{Session, SessionStream},
        slot::Slot,
        subscribe::{Sessions, SubscribeHandler},
    },
    status::embed_routing_error,
    token::Token,
};

// The one point where BPA routing errors become gRPC statuses. The
// typed discriminator is embedded so the SDK can recover the exact
// variant past the coarse code.
fn routing_status(error: Error) -> Status {
    let status = match &error {
        Error::AlreadyExists(_) => Status::already_exists(error.to_string()),
        Error::Disconnected => Status::unavailable("Unregistered"),
        Error::NullNextHop | Error::ViaOwnNode(_) => Status::invalid_argument(error.to_string()),
        // The internal chain may carry host detail an untrusted peer
        // must never see: log it server-side and ship a generic status.
        Error::Internal(e) => {
            error!("internal routing error: {e}");
            Status::internal("internal error")
        }
    };
    embed_routing_error(status, &error)
}

// The component as the BPA sees it.
// The BPA never calls back into a routing agent, so the session's down
// direction carries only the Registration and then anchors liveness.
struct GrpcRoutingAgent {
    session: Session<SubscribeResponse>,
    sink: Slot<Arc<dyn RoutingSink>>,
}

impl GrpcRoutingAgent {
    fn new(session: Session<SubscribeResponse>) -> Self {
        Self {
            session,
            sink: Slot::new(),
        }
    }
}

impl SubscribeHandler for GrpcRoutingAgent {
    type Event = SubscribeResponse;
    type Request = SubscribeRequest;
    // The name the agent registers under.
    type Register = String;

    const LABEL: &'static str = "routing";

    // A routing agent receives no down-events, so the channel only has
    // to carry the one Registration the stream yields structurally.
    const EVENT_DEPTH: usize = 1;

    fn session(&self) -> &Session<SubscribeResponse> {
        &self.session
    }

    fn into_register(request: SubscribeRequest) -> Option<String> {
        let Some(subscribe_request::Request::Register(register)) = request.request else {
            return None;
        };
        Some(register.name)
    }

    async fn register(
        bpa: &Arc<dyn BpaRegistration>,
        name: String,
        cancel: CancellationToken,
        events: mpsc::Sender<Result<SubscribeResponse, Status>>,
    ) -> Result<(Arc<Self>, SubscribeResponse), Status> {
        // Minted before the BPA sees the component: the session must be
        // able to carry the Registration the moment it completes.
        let token = Token::mint(&format!("{}:{name}", Self::LABEL));
        let agent = Arc::new(GrpcRoutingAgent::new(Session::new(
            token.clone(),
            cancel,
            events,
        )));

        let node_ids = bpa
            .register_routing_agent(name, agent.clone())
            .await
            .map_err(routing_status)?;

        Ok((
            agent,
            SubscribeResponse {
                event: Some(subscribe_response::Event::Registration(Registration {
                    node_ids: node_ids.iter().map(ToString::to_string).collect(),
                    session_token: token.into(),
                })),
            },
        ))
    }

    async fn on_request(&self, request: SubscribeRequest) -> ControlFlow<()> {
        match request.request {
            // The one message that ends a subscription client-side.
            Some(subscribe_request::Request::Unregister(_)) => ControlFlow::Break(()),
            // Not Debug-formatted: the message carries the session
            // token, which must never reach the logs.
            Some(subscribe_request::Request::Register(_)) | None => {
                warn!("Ignoring unexpected message on the session stream");
                ControlFlow::Continue(())
            }
        }
    }

    async fn unregister(&self) {
        if let Some(sink) = self.sink.peek() {
            sink.unregister().await;
        }
    }
}

#[async_trait]
impl RoutingAgent for GrpcRoutingAgent {
    async fn on_register(&self, sink: Box<dyn RoutingSink>, _node_ids: &[NodeId]) {
        self.sink.set(Arc::from(sink));
    }

    async fn on_unregister(&self) {
        // The session task catches this and runs the one exit sequence.
        self.session.abort();
    }
}

/// The routing agent surface. Shutting down the pool given to
/// [`new`](Self::new) tears the subscriptions and drives
/// unregistration, so shut it down only after the transport has stopped
/// accepting.
#[derive(Clone)]
pub struct RoutingAgentServiceImpl {
    sessions: Sessions<GrpcRoutingAgent>,
}

impl RoutingAgentServiceImpl {
    /// Serves the routing surface of `bpa`.
    pub fn new(bpa: Arc<dyn BpaRegistration>, tasks: TaskPool) -> Self {
        Self {
            sessions: Sessions::new(bpa, tasks),
        }
    }
}

#[async_trait]
impl RoutingAgentService for RoutingAgentServiceImpl {
    type SubscribeStream = SessionStream<SubscribeResponse>;

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn subscribe(
        &self,
        request: Request<Streaming<SubscribeRequest>>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        self.sessions.subscribe(request.into_inner()).await
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn add_route(
        &self,
        request: Request<AddRouteRequest>,
    ) -> Result<Response<AddRouteResponse>, Status> {
        let AddRouteRequest {
            session_token,
            pattern,
            action,
            priority,
        } = request.into_inner();
        let agent = self.sessions.resolve(session_token)?;
        let pattern: EidPattern = pattern
            .parse()
            .map_err(|e| Status::invalid_argument(format!("Invalid pattern: {e}")))?;
        let action: routing::RouteAction = action
            .and_then(|a| a.action)
            .ok_or_else(|| Status::invalid_argument("Missing route action"))?
            .try_into()?;

        let added = agent
            .sink
            .get()?
            .add_route(pattern, action, priority)
            .await
            .map_err(routing_status)?;
        Ok(Response::new(AddRouteResponse { added }))
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn remove_route(
        &self,
        request: Request<RemoveRouteRequest>,
    ) -> Result<Response<RemoveRouteResponse>, Status> {
        let RemoveRouteRequest {
            session_token,
            pattern,
            action,
            priority,
        } = request.into_inner();
        let agent = self.sessions.resolve(session_token)?;
        let pattern: EidPattern = pattern
            .parse()
            .map_err(|e| Status::invalid_argument(format!("Invalid pattern: {e}")))?;
        let action: routing::RouteAction = action
            .and_then(|a| a.action)
            .ok_or_else(|| Status::invalid_argument("Missing route action"))?
            .try_into()?;

        let removed = agent
            .sink
            .get()?
            .remove_route(&pattern, &action, priority)
            .await
            .map_err(routing_status)?;
        Ok(Response::new(RemoveRouteResponse { removed }))
    }
}

// The wire against a real BPA: the generated client, a port-0 listener,
// and the route doors exercised end to end.
#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use hardy_bpa::{Bytes, bpa::Bpa};
    #[cfg(feature = "client")]
    use hardy_bpv7::{eid::NodeId, status_report::ReasonCode};
    #[cfg(feature = "client")]
    use hardy_eid_patterns::EidPattern;
    use tokio::sync::mpsc::Sender;
    use tokio_stream::wrappers::ReceiverStream;
    use tonic::{
        Code,
        transport::{Channel, Server},
    };

    use super::{
        super::tests::{build_bpa, ipn1, serve, timeout, wait_torn_down},
        *,
    };
    #[cfg(feature = "client")]
    use crate::client::BpaClient;
    use crate::routing::{
        Drop, Register, RouteAction, Unregister, route_action::Action,
        routing_agent_service_client::RoutingAgentServiceClient,
        routing_agent_service_server::RoutingAgentServiceServer,
    };
    use crate::server::subscribe::Sessions;

    struct Harness {
        bpa: Arc<Bpa>,
        // Held live: dropping the pool would tear the sessions.
        #[expect(dead_code, reason = "held for its liveness")]
        tasks: TaskPool,
        client: RoutingAgentServiceClient<Channel>,
        #[cfg_attr(
            not(feature = "client"),
            expect(dead_code, reason = "read by the client SDK test")
        )]
        address: SocketAddr,
        // The session index, for the teardown barrier.
        sessions: Sessions<GrpcRoutingAgent>,
    }

    // A running BPA (node ipn:1) behind the surface on a port-0 listener,
    // plus a connected generated client.
    async fn harness() -> Harness {
        let bpa = build_bpa(ipn1(), false).await;

        let tasks = TaskPool::new();
        let service_impl = RoutingAgentServiceImpl::new(bpa.clone(), tasks.clone());
        let sessions = service_impl.sessions.clone();
        let service = RoutingAgentServiceServer::new(service_impl);
        let address = serve(Server::builder().add_service(service)).await;

        let client = RoutingAgentServiceClient::connect(format!("http://{address}"))
            .await
            .unwrap();
        Harness {
            bpa,
            tasks,
            client,
            address,
            sessions,
        }
    }

    struct Registered {
        requests_tx: Sender<SubscribeRequest>,
        events: Streaming<SubscribeResponse>,
        node_ids: Vec<String>,
        token: Bytes,
    }

    // Opens a session and completes the registration handshake.
    async fn register(client: &mut RoutingAgentServiceClient<Channel>, name: &str) -> Registered {
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
            panic!("expected the Registration event first, got {event:?}");
        };
        assert!(!registration.session_token.is_empty());

        Registered {
            requests_tx,
            events,
            node_ids: registration.node_ids,
            token: registration.session_token,
        }
    }

    // A `via` route action to a remote next hop.
    fn via(eid: &str) -> RouteAction {
        RouteAction {
            action: Some(Action::Via(eid.to_string())),
        }
    }

    // A `drop` route action carrying an explicit reason code.
    fn drop_with_reason(reason_code: u64) -> RouteAction {
        RouteAction {
            action: Some(Action::Drop(Drop {
                reason_code: Some(reason_code),
            })),
        }
    }

    async fn add_route(
        client: &mut RoutingAgentServiceClient<Channel>,
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
        client: &mut RoutingAgentServiceClient<Channel>,
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
        assert_eq!(registered.node_ids.len(), 1);
        assert!(registered.node_ids[0].starts_with("ipn:1"));

        // A duplicate name is rejected as the local registry rejects it.
        let (requests_tx, requests_rx) = mpsc::channel(4);
        requests_tx
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Register(Register {
                    name: "test-agent".to_string(),
                })),
            })
            .await
            .unwrap();
        let status = harness
            .client
            .subscribe(ReceiverStream::new(requests_rx))
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::AlreadyExists);

        harness.bpa.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn routes_are_added_and_removed_once() {
        let mut harness = harness().await;
        let registered = register(&mut harness.client, "test-agent").await;

        // Newly installed, then a duplicate is a no-op.
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

        // Removed once, then unknown.
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

    // RFC 9171 reserves status-report reason code 255: the wire refuses
    // it, while an unassigned code is carried through.
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

        // The client vanishes without Unregister. The teardown signal
        // fires once the token is gone and the agent unregistered, so
        // both the rejection and the re-registration below are race-free.
        let mut torn = harness.sessions.torn_down();
        drop(registered.events);
        drop(registered.requests_tx);
        wait_torn_down(&mut torn, &registered.token).await;

        // The token is dead.
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

        // Teardown freed the name, so this succeeds on the first try.
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
        let mut torn = harness.sessions.torn_down();

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

        // Teardown runs after the stream closes, so the signal is what
        // makes the rejection below race-free.
        wait_torn_down(&mut torn, &registered.token).await;
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

    // A routing agent behind the client SDK: it stores its sink and drives
    // the RIB through it.
    #[cfg(feature = "client")]
    struct SdkAgent {
        sink: hardy_async::sync::spin::Once<Box<dyn RoutingSink>>,
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
        let client =
            BpaClient::new(format!("http://{}", harness.address), TaskPool::new()).unwrap();

        let agent = Arc::new(SdkAgent {
            sink: hardy_async::sync::spin::Once::new(),
        });
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

        // The reserved drop reason is refused by the sink before it
        // reaches the wire, the same refusal the server gives it.
        let reserved = routing::RouteAction::Drop(Some(ReasonCode::Unassigned(255)));
        assert!(matches!(
            sink.add_route(pattern.clone(), reserved, 60).await,
            Err(routing::Error::Internal(_))
        ));

        sink.unregister().await;
        harness.bpa.shutdown().await;
    }
}
