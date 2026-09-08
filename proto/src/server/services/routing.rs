// The routing surface: the `hardy.routing.v1` wire served against the routing
// surface of a BPA. This is the template minus the data plane: routing
// agents are push-only, so the session carries only the Registration
// event and then anchors the registration's liveness, and the two
// token-gated doors (AddRoute / RemoveRoute) drive the RIB directly.
// Declarations are ordered define-before-reference: the wire
// conversions, the component as the BPA sees it, then the rpc service.

use std::sync::Arc;

use hardy_async::TaskPool;
use hardy_bpa::{
    async_trait,
    bpa::BpaRegistration,
    routing::{self, Error, RoutingAgent, RoutingSink},
};
use hardy_eid_patterns::EidPattern;
use tokio::sync::mpsc;
use tonic::{Request, Response, Status, Streaming};
use tracing::error;
#[cfg(feature = "instrument")]
use tracing::instrument;

use super::{Bridge, Component, SinkSlot};
use crate::{
    error_status::embed_routing_error,
    routing::{
        AddRouteRequest, AddRouteResponse, Registration, RemoveRouteRequest, RemoveRouteResponse,
        SubscribeRequest, SubscribeResponse, routing_agent_service_server::RoutingAgentService,
        subscribe_request, subscribe_response,
    },
    server::session::{Session, SessionStream},
};

// The one point where BPA routing errors become gRPC statuses. The typed
// discriminator is embedded on the way out so the SDK can recover the
// exact variant past the coarse code.
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

// -------------------------------------------------------------------
// The component as the BPA sees it
// -------------------------------------------------------------------

// A routing agent needs no event channel of its own: the BPA never calls
// back into it, so the session's down direction carries only the initial
// Registration and then anchors liveness. The `sink` is what the doors
// drive.
struct GrpcRoutingAgent {
    session: Session<SubscribeResponse>,
    sink: SinkSlot<dyn RoutingSink>,
}

impl GrpcRoutingAgent {
    fn new(session: Session<SubscribeResponse>) -> Self {
        Self {
            session,
            sink: SinkSlot::new(),
        }
    }
}

impl Component for GrpcRoutingAgent {
    type Event = SubscribeResponse;

    fn session(&self) -> &Session<SubscribeResponse> {
        &self.session
    }

    async fn unregister_sink(&self) {
        if let Some(sink) = self.sink.peek() {
            sink.unregister().await;
        }
    }
}

#[async_trait]
impl RoutingAgent for GrpcRoutingAgent {
    async fn on_register(&self, sink: Box<dyn RoutingSink>, _node_ids: &[hardy_bpv7::eid::NodeId]) {
        self.sink.set(sink);
    }

    async fn on_unregister(&self) {
        // BPA-initiated teardown: pull the trigger; the session task
        // catches it and runs the one exit sequence.
        self.session.abort();
    }
}

// -------------------------------------------------------------------
// Control plane: the session
// -------------------------------------------------------------------

/// The routing agent bridge. Shutting down the pool given to
/// [`new`](Self::new) tears the sessions and drives unregistration, so
/// shut it down only after the transport has stopped accepting.
#[derive(Clone)]
pub struct RoutingAgentServiceImpl {
    bridge: Bridge<GrpcRoutingAgent>,
}

impl RoutingAgentServiceImpl {
    /// Bridges the routing surface of `bpa`.
    pub fn new(bpa: Arc<dyn BpaRegistration>, tasks: TaskPool) -> Self {
        Self {
            bridge: Bridge::new(bpa, tasks),
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
        let mut requests = request.into_inner();

        // The wire requires Register first, sent without waiting for
        // response headers, so the handshake runs inline and a failed
        // registration is a plain error on the call.
        let Some(subscribe_request::Request::Register(register)) =
            requests.message().await?.and_then(|r| r.request)
        else {
            return Err(Status::invalid_argument(
                "The first message must be Register",
            ));
        };

        // The token is minted before the BPA sees the component: the
        // session must be able to carry the Registration event the moment
        // registration completes.
        let token = self
            .bridge
            .sessions
            .mint(&format!("routing:{}", register.name));
        // A routing agent receives no down-events, so the channel only has
        // to carry the one Registration the stream yields structurally.
        let (events_tx, events_rx) = mpsc::channel(1);
        let agent = Arc::new(GrpcRoutingAgent::new(Session::new(
            token.clone(),
            self.bridge.tasks.child_token(),
            events_tx,
        )));

        let node_ids = self
            .bridge
            .bpa
            .register_routing_agent(register.name, agent.clone())
            .await
            .map_err(routing_status)?;

        // The stream yields the Registration first, then anchors the
        // session's liveness: routing agents receive no further events.
        let registration = SubscribeResponse {
            event: Some(subscribe_response::Event::Registration(Registration {
                node_ids: node_ids.iter().map(ToString::to_string).collect(),
                session_token: token.into(),
            })),
        };
        Ok(Response::new(self.bridge.open_session(
            agent,
            registration,
            events_rx,
            requests,
            "routing_session",
        )))
    }

    // ---------------------------------------------------------------
    // Routes: token-gated unary doors
    // ---------------------------------------------------------------

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
        let agent = self.bridge.sessions.resolve(session_token)?;
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
        let agent = self.bridge.sessions.resolve(session_token)?;
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
    use crate::routing::{
        Drop, Register, RouteAction, Unregister, route_action::Action,
        routing_agent_service_client::RoutingAgentServiceClient,
        routing_agent_service_server::RoutingAgentServiceServer,
    };
    use crate::server::session::Sessions;

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
        address: std::net::SocketAddr,
        // The session index, for the teardown barrier.
        sessions: Arc<Sessions<GrpcRoutingAgent>>,
    }

    // A running BPA (node ipn:1) behind the bridge on a port-0 listener,
    // plus a connected generated client.
    async fn harness() -> Harness {
        let bpa = build_bpa(ipn1(), false).await;

        let tasks = TaskPool::new();
        let service_impl = RoutingAgentServiceImpl::new(bpa.clone(), tasks.clone());
        let sessions = service_impl.bridge.sessions.clone();
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
        requests: Sender<SubscribeRequest>,
        events: Streaming<SubscribeResponse>,
        node_ids: Vec<String>,
        token: Bytes,
    }

    // Opens a session and completes the registration handshake.
    async fn register(client: &mut RoutingAgentServiceClient<Channel>, name: &str) -> Registered {
        let (requests, rx) = mpsc::channel(4);
        requests
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Register(Register {
                    name: name.to_string(),
                })),
            })
            .await
            .unwrap();

        let mut events = client
            .subscribe(ReceiverStream::new(rx))
            .await
            .unwrap()
            .into_inner();
        let event = timeout(events.message()).await.unwrap().unwrap();
        let Some(subscribe_response::Event::Registration(registration)) = event.event else {
            panic!("expected the Registration event first, got {event:?}");
        };
        assert!(!registration.session_token.is_empty());

        Registered {
            requests,
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
        let (requests, rx) = mpsc::channel(4);
        requests
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Register(Register {
                    name: "test-agent".to_string(),
                })),
            })
            .await
            .unwrap();
        let status = harness
            .client
            .subscribe(ReceiverStream::new(rx))
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

        // The client vanishes without Unregister: dropping the rpc's
        // streams is caught by the response-stream guard and the request
        // half-close, and the session tears down. The teardown signal
        // fires once the token is gone AND the agent is unregistered from
        // the BPA, so both the rejection and the re-registration below
        // are race-free.
        let mut torn = harness.sessions.torn_down();
        drop(registered.events);
        drop(registered.requests);
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

        // Teardown also unregistered the agent from the BPA, so the name
        // is free for a new registration, which now succeeds on the first
        // try.
        let (requests, rx) = mpsc::channel(4);
        requests
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Register(Register {
                    name: "test-agent".to_string(),
                })),
            })
            .await
            .unwrap();
        harness
            .client
            .subscribe(ReceiverStream::new(rx))
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
            .requests
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

        // The token dies in the session task's teardown, which runs after
        // the stream closes; wait for the teardown signal, so the
        // rejection below is asserted without a race.
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
            crate::client::BpaClient::new(format!("http://{}", harness.address), TaskPool::new())
                .unwrap();

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
