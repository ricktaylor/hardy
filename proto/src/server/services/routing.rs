// The routing surface: the `routing.v1` wire served against the routing
// surface of a BPA. This is the template minus the data plane: routing
// agents are push-only, so the session carries only the Registration
// event and then anchors the registration's liveness, and the two
// token-gated doors (AddRoute / RemoveRoute) drive the RIB directly.
// Declarations are ordered define-before-reference: the wire
// conversions, the component as the BPA sees it, then the rpc service.

use std::sync::Arc;

use hardy_async::{TaskPool, sync::spin::Once};
use hardy_bpa::{
    async_trait,
    bpa::BpaRegistration,
    routing::{self, RoutingAgent, RoutingSink},
};
use hardy_eid_patterns::EidPattern;
use tokio::sync::mpsc;
use tonic::{Request, Response, Status, Streaming};
#[cfg(feature = "instrument")]
use tracing::instrument;

use super::watch_session;
use crate::routing::{
    AddRouteRequest, AddRouteResponse, Registration, RemoveRouteRequest, RemoveRouteResponse,
    SubscribeRequest, SubscribeResponse, routing_agent_service_server::RoutingAgentService,
    subscribe_request, subscribe_response,
};
use crate::server::session::{Session, SessionStream, Sessions};
use crate::server::token::Signer;

// The one point where BPA routing errors become gRPC statuses.
fn routing_status(error: routing::Error) -> Status {
    use routing::Error;
    match error {
        Error::AlreadyExists(_) => Status::already_exists(error.to_string()),
        Error::Disconnected => Status::unavailable("Unregistered"),
        Error::NullNextHop | Error::ViaOwnNode(_) => Status::invalid_argument(error.to_string()),
        Error::Internal(e) => Status::from_error(e),
    }
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
    sink: Once<Arc<dyn RoutingSink>>,
}

impl GrpcRoutingAgent {
    fn new(session: Session<SubscribeResponse>) -> Self {
        Self {
            session,
            sink: Once::new(),
        }
    }

    fn sink(&self) -> Result<Arc<dyn RoutingSink>, Status> {
        self.sink
            .get()
            .cloned()
            .ok_or_else(|| Status::unavailable("Unregistered"))
    }
}

#[async_trait]
impl RoutingAgent for GrpcRoutingAgent {
    async fn on_register(&self, sink: Box<dyn RoutingSink>, _node_ids: &[hardy_bpv7::eid::NodeId]) {
        self.sink.call_once(|| Arc::from(sink));
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
    bpa: Arc<dyn BpaRegistration>,
    tasks: TaskPool,
    sessions: Arc<Sessions<GrpcRoutingAgent>>,
}

impl RoutingAgentServiceImpl {
    /// Bridges the routing surface of `bpa`, minting session tokens with
    /// the server-wide `signer`.
    pub fn new(bpa: Arc<dyn BpaRegistration>, tasks: TaskPool, signer: Signer) -> Self {
        Self {
            bpa,
            tasks,
            sessions: Arc::new(Sessions::new(signer)),
        }
    }

    // Unregisters one session, however it ended. The BPA withdraws the
    // agent's routes when the sink unregisters; a repeat unregister
    // no-ops inside the sink.
    async fn unregister_session(&self, agent: Arc<GrpcRoutingAgent>) {
        // Ordered by what the client must observe as done. Retire the
        // token, then unregister from the BPA (which frees the name
        // before firing on_unregister), then close the stream last: the
        // client sees teardown via the stream closing, so once it does
        // the token is dead and the name is reusable.
        self.sessions.remove(agent.session.token());
        if let Some(sink) = agent.sink.get() {
            sink.unregister().await;
        }
        agent.session.abort();
        // The teardown barrier for tests: the session is fully retired.
        #[cfg(test)]
        self.sessions.signal_torn_down(agent.session.token());
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
        let token = self.sessions.mint(&format!("routing:{}", register.name));
        // A routing agent receives no down-events, so the channel only has
        // to carry the one Registration the stream yields structurally.
        let (events_tx, events_rx) = mpsc::channel(1);
        let agent = Arc::new(GrpcRoutingAgent::new(Session::new(
            token.clone(),
            self.tasks.child_token(),
            events_tx,
        )));

        let node_ids = self
            .bpa
            .register_routing_agent(register.name, agent.clone())
            .await
            .map_err(routing_status)?;

        // Published before the client can know its token.
        self.sessions.publish(token.clone(), agent.clone());

        // The one task a session owns: it waits out the session's life,
        // then unregisters it. The session ends on Unregister or
        // half-close, or on the trigger: the stream's guard (the rpc
        // dying), `on_unregister`, or pool shutdown.
        let cancelled = agent.session.cancellation();

        // The stream yields the Registration first, then anchors the
        // session's liveness: routing agents receive no further events.
        let registration = SubscribeResponse {
            event: Some(subscribe_response::Event::Registration(Registration {
                node_ids: node_ids.iter().map(ToString::to_string).collect(),
                session_token: token.into(),
            })),
        };
        let stream = agent.session.stream(registration, events_rx);
        let service_impl = self.clone();
        hardy_async::spawn!(self.tasks, "routing_session", async move {
            watch_session(cancelled, requests).await;
            service_impl.unregister_session(agent).await;
        });

        Ok(Response::new(stream))
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
        let agent = self.sessions.resolve(session_token)?;
        let pattern: EidPattern = pattern
            .parse()
            .map_err(|e| Status::invalid_argument(format!("Invalid pattern: {e}")))?;
        let action: routing::RouteAction = action
            .and_then(|a| a.action)
            .ok_or_else(|| Status::invalid_argument("Missing route action"))?
            .try_into()?;

        let added = agent
            .sink()?
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
            .sink()?
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
    use std::time::Duration;

    use hardy_bpa::{Bytes, bpa::Bpa, node_ids::NodeIds};
    use hardy_bpv7::eid::{IpnNodeId, NodeId};
    #[cfg(feature = "client")]
    use hardy_eid_patterns::EidPattern;
    use tokio::net::TcpListener;
    use tokio::sync::mpsc::Sender;
    use tokio_stream::wrappers::ReceiverStream;
    use tonic::Code;
    use tonic::transport::{Channel, Server, server::TcpIncoming};

    use crate::routing::{
        Register, RouteAction, Unregister, route_action::Action,
        routing_agent_service_client::RoutingAgentServiceClient,
        routing_agent_service_server::RoutingAgentServiceServer,
    };

    use super::*;

    async fn timeout<F: Future>(future: F) -> F::Output {
        tokio::time::timeout(Duration::from_secs(10), future)
            .await
            .expect("test timed out")
    }

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
        let node_ids = NodeIds::try_from(
            [NodeId::Ipn(IpnNodeId {
                allocator_id: 0,
                node_number: 1,
            })]
            .as_slice(),
        )
        .unwrap();
        let bpa = Arc::new(Bpa::builder().node_ids(node_ids).build().await.unwrap());
        bpa.start(false);

        let tasks = TaskPool::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let incoming = TcpIncoming::from(listener).with_nodelay(Some(true));

        let service_impl = RoutingAgentServiceImpl::new(bpa.clone(), tasks.clone(), Signer::new());
        let sessions = service_impl.sessions.clone();
        let service = RoutingAgentServiceServer::new(service_impl);
        tokio::spawn(
            Server::builder()
                .add_service(service)
                .serve_with_incoming(incoming),
        );

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
        // are race-free (the timeout only bounds a regression).
        let mut torn = harness.sessions.torn_down();
        drop(registered.events);
        drop(registered.requests);
        timeout(async { while Bytes::from(torn.recv().await.unwrap()) != registered.token {} })
            .await;

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
        // rejection below is asserted without a race. The timeout only
        // bounds a regression.
        timeout(async { while Bytes::from(torn.recv().await.unwrap()) != registered.token {} })
            .await;
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
        let remote =
            crate::client::BpaClient::new(format!("http://{}", harness.address), TaskPool::new())
                .unwrap();

        let agent = Arc::new(SdkAgent {
            sink: hardy_async::sync::spin::Once::new(),
        });
        let node_ids = remote
            .register_routing_agent("sdk-agent".to_string(), agent.clone())
            .await
            .unwrap();
        assert_eq!(node_ids.len(), 1);

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

        sink.unregister().await;
        harness.bpa.shutdown().await;
    }
}
