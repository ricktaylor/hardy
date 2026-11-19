use hardy_bpa::{
    node_ids::NodeIds,
    routing::{Context, RegistrarExt, Sink, factory_fn},
};
use hardy_bpv7::eid::NodeId;

use super::*;
use proto::routing::*;

fn routing_error_to_status(e: hardy_bpa::routing::Error) -> tonic::Status {
    match e {
        hardy_bpa::routing::Error::NullNextHop | hardy_bpa::routing::Error::ViaOwnNode(_) => {
            tonic::Status::invalid_argument(e.to_string())
        }
        hardy_bpa::routing::Error::DuplicateName(_)
        | hardy_bpa::routing::Error::Unregistering(_) => {
            tonic::Status::already_exists(e.to_string())
        }
        hardy_bpa::routing::Error::Disconnected => tonic::Status::unavailable(e.to_string()),
        other => tonic::Status::internal(other.to_string()),
    }
}

// The BPA's end of a remote routing agent: the BPA reaches that agent
// through the proxy, and the agent reaches the BPA back through the sink.
struct Agent {
    sink: Arc<dyn Sink>,
    node_ids: Arc<NodeIds>,
    proxy: Arc<RpcProxy<Result<BpaToAgent, tonic::Status>, AgentToBpa>>,
}

#[async_trait]
impl hardy_bpa::routing::Agent for Agent {
    async fn unregister(&self) -> hardy_bpa::routing::Result<()> {
        self.proxy.shutdown().await;
        Ok(())
    }
}

// The far agent's end of the session: every request it makes lands on
// the sink its registration handed back.
//
// `RouteAction` is also a wire message of this module's proto, so the
// BPA's type of that name stays fully qualified.
struct Handler {
    sink: Arc<dyn Sink>,
}

impl Handler {
    async fn add_route(
        &self,
        request: AddRouteRequest,
    ) -> Result<bpa_to_agent::Msg, tonic::Status> {
        let pattern = request
            .pattern
            .parse()
            .map_err(|e| tonic::Status::invalid_argument(format!("Invalid EID pattern: {e}")))?;

        let action: hardy_bpa::routing::RouteAction = request
            .action
            .ok_or(tonic::Status::invalid_argument("Missing action"))?
            .try_into()?;

        let added = self
            .sink
            .add_route(pattern, action, request.priority)
            .await
            .map_err(routing_error_to_status)?;

        Ok(bpa_to_agent::Msg::AddRoute(AddRouteResponse { added }))
    }

    async fn remove_route(
        &self,
        request: RemoveRouteRequest,
    ) -> Result<bpa_to_agent::Msg, tonic::Status> {
        let pattern = request
            .pattern
            .parse()
            .map_err(|e| tonic::Status::invalid_argument(format!("Invalid EID pattern: {e}")))?;

        let action: hardy_bpa::routing::RouteAction = request
            .action
            .ok_or(tonic::Status::invalid_argument("Missing action"))?
            .try_into()?;

        let removed = self
            .sink
            .remove_route(&pattern, &action, request.priority)
            .await
            .map_err(routing_error_to_status)?;

        Ok(bpa_to_agent::Msg::RemoveRoute(RemoveRouteResponse {
            removed,
        }))
    }
}

#[async_trait]
impl ProxyHandler for Handler {
    type SMsg = bpa_to_agent::Msg;
    type RMsg = agent_to_bpa::Msg;

    async fn on_notify(&self, msg: Self::RMsg) -> Option<Self::SMsg> {
        let msg = match msg {
            agent_to_bpa::Msg::AddRoute(msg) => self.add_route(msg).await,
            agent_to_bpa::Msg::RemoveRoute(msg) => self.remove_route(msg).await,
            _ => {
                warn!("Ignoring unsolicited response: {msg:?}");
                return None;
            }
        };

        match msg {
            Ok(msg) => Some(msg),
            Err(e) => Some(bpa_to_agent::Msg::Status(e.into())),
        }
    }

    async fn on_close(&self) {
        // The far agent is gone: unregister, which withdraws its routes
        // and takes the agent down with the session.
        self.sink.unregister().await;
    }
}

pub struct Service {
    bpa: Arc<dyn hardy_bpa::routing::Registrar>,
    session_tasks: hardy_async::TaskPool,
    channel_size: usize,
}

#[async_trait]
impl routing_agent_server::RoutingAgent for Service {
    type RegisterStream = tokio_stream::wrappers::ReceiverStream<Result<BpaToAgent, tonic::Status>>;

    async fn register(
        &self,
        request: tonic::Request<tonic::Streaming<AgentToBpa>>,
    ) -> Result<tonic::Response<Self::RegisterStream>, tonic::Status> {
        let (channel_sender, rx) = tokio::sync::mpsc::channel(self.channel_size);
        let channel_receiver = request.into_inner();

        // Spawn the registration handshake and proxy — we must return the
        // response stream immediately so the client can start sending messages.
        let bpa = self.bpa.clone();
        hardy_async::spawn!(self.session_tasks, "routing_session", async move {
            run_routing_session(channel_sender, channel_receiver, bpa).await;
        });

        Ok(tonic::Response::new(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ))
    }
}

async fn run_routing_session(
    channel_sender: tokio::sync::mpsc::Sender<Result<BpaToAgent, tonic::Status>>,
    channel_receiver: tonic::Streaming<AgentToBpa>,
    bpa: Arc<dyn hardy_bpa::routing::Registrar>,
) {
    let (proxy, msg_id, opening) = match RpcProxy::accept(channel_sender, channel_receiver).await {
        Ok(session) => session,
        Err(e) => {
            warn!("Routing agent session failed to open: {e}");
            return;
        }
    };

    match register(&*bpa, opening, &proxy).await {
        Ok(agent) => {
            let response = RegisterRoutingAgentResponse {
                node_ids: Vec::<NodeId>::from(agent.node_ids.as_ref())
                    .into_iter()
                    .map(|node_id| node_id.to_string())
                    .collect(),
            };
            let handler = Handler {
                sink: agent.sink.clone(),
            };
            proxy.open(
                msg_id,
                bpa_to_agent::Msg::Register(response),
                Box::new(handler),
            );
        }
        Err(status) => {
            warn!("Routing agent registration failed: {status}");
            proxy.fail(status).await;
        }
    }
}

// Registers the agent the opening message asks for, built around the
// proxy the BPA reaches it through.
async fn register(
    bpa: &dyn hardy_bpa::routing::Registrar,
    opening: agent_to_bpa::Msg,
    proxy: &Arc<RpcProxy<Result<BpaToAgent, tonic::Status>, AgentToBpa>>,
) -> Result<Arc<Agent>, tonic::Status> {
    let agent_to_bpa::Msg::Register(request) = opening else {
        warn!("Routing agent sent incorrect message: {opening:?}");
        return Err(tonic::Status::internal(format!(
            "Unexpected response: {opening:?}"
        )));
    };

    bpa.register_routing_agent(
        &request.name,
        factory_fn(|cx: Context| {
            Ok(Arc::new(Agent {
                sink: cx.sink,
                node_ids: cx.node_ids,
                proxy: proxy.clone(),
            }))
        }),
    )
    .await
    .map_err(routing_error_to_status)
}

/// Create a new routing agent gRPC service.
pub fn new_routing_agent_service(
    bpa: Arc<dyn hardy_bpa::routing::Registrar>,
    tasks: &hardy_async::TaskPool,
) -> routing_agent_server::RoutingAgentServer<Service> {
    routing_agent_server::RoutingAgentServer::new(Service {
        bpa,
        session_tasks: tasks.clone(),
        channel_size: 16,
    })
    .max_encoding_message_size(crate::MAX_MESSAGE_SIZE)
    .max_decoding_message_size(crate::MAX_MESSAGE_SIZE)
}

#[cfg(test)]
mod tests {
    use hardy_eid_patterns::EidPattern;

    use super::*;

    // A sink that records the routes the agent pushes at it, and answers
    // each with a canned verdict.
    struct MockSink {
        verdict: bool,
        routes: Mutex<Vec<(EidPattern, hardy_bpa::routing::RouteAction, u32)>>,
    }

    impl MockSink {
        fn new(verdict: bool) -> Arc<Self> {
            Arc::new(Self {
                verdict,
                routes: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl Sink for MockSink {
        async fn unregister(&self) {}

        async fn add_route(
            &self,
            pattern: EidPattern,
            action: hardy_bpa::routing::RouteAction,
            priority: u32,
        ) -> hardy_bpa::routing::Result<bool> {
            self.routes.lock().push((pattern, action, priority));
            Ok(self.verdict)
        }

        async fn remove_route(
            &self,
            pattern: &EidPattern,
            action: &hardy_bpa::routing::RouteAction,
            priority: u32,
        ) -> hardy_bpa::routing::Result<bool> {
            self.routes
                .lock()
                .push((pattern.clone(), action.clone(), priority));
            Ok(self.verdict)
        }
    }

    fn via(eid: &str) -> RouteAction {
        RouteAction {
            action: Some(route_action::Action::Via(eid.to_string())),
        }
    }

    // SRV-02: an add_route request reaches the sink as the route it
    // names, and the sink's verdict rides the response.
    #[tokio::test]
    async fn srv_02_added_route_reaches_the_sink() {
        let sink = MockSink::new(true);
        let handler = Handler { sink: sink.clone() };

        let msg = handler
            .add_route(AddRouteRequest {
                pattern: "ipn:*.*".to_string(),
                action: Some(via("ipn:2.0")),
                priority: 7,
            })
            .await
            .expect("a well-formed request is accepted");

        assert_eq!(
            msg,
            bpa_to_agent::Msg::AddRoute(AddRouteResponse { added: true })
        );
        assert_eq!(
            sink.routes.lock().as_slice(),
            [(
                "ipn:*.*".parse::<EidPattern>().unwrap(),
                hardy_bpa::routing::RouteAction::Via("ipn:2.0".parse().unwrap()),
                7
            )]
        );
    }

    // SRV-03: a remove_route request reaches the sink the same way, and a
    // sink that found nothing to remove answers `removed: false`.
    #[tokio::test]
    async fn srv_03_removed_route_reaches_the_sink() {
        let sink = MockSink::new(false);
        let handler = Handler { sink: sink.clone() };

        let msg = handler
            .remove_route(RemoveRouteRequest {
                pattern: "ipn:*.*".to_string(),
                action: Some(via("ipn:2.0")),
                priority: 7,
            })
            .await
            .expect("a well-formed request is accepted");

        assert_eq!(
            msg,
            bpa_to_agent::Msg::RemoveRoute(RemoveRouteResponse { removed: false })
        );
        assert_eq!(
            sink.routes.lock().as_slice(),
            [(
                "ipn:*.*".parse::<EidPattern>().unwrap(),
                hardy_bpa::routing::RouteAction::Via("ipn:2.0".parse().unwrap()),
                7
            )]
        );
    }

    // SRV-04: a request the wire layer cannot turn into a route is
    // refused as invalid, and never reaches the sink.
    #[tokio::test]
    async fn srv_04_malformed_route_request_is_refused() {
        let sink = MockSink::new(true);
        let handler = Handler { sink: sink.clone() };

        let unparseable_pattern = handler
            .add_route(AddRouteRequest {
                pattern: "not an EID pattern".to_string(),
                action: Some(via("ipn:2.0")),
                priority: 0,
            })
            .await
            .expect_err("the pattern does not parse");
        assert_eq!(unparseable_pattern.code(), tonic::Code::InvalidArgument);

        let missing_action = handler
            .add_route(AddRouteRequest {
                pattern: "ipn:*.*".to_string(),
                action: None,
                priority: 0,
            })
            .await
            .expect_err("the request carries no action");
        assert_eq!(missing_action.code(), tonic::Code::InvalidArgument);

        assert!(
            sink.routes.lock().is_empty(),
            "a refused request must not reach the sink"
        );
    }
}
