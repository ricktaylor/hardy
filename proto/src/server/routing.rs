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
        hardy_bpa::routing::Error::DuplicateName(_) | hardy_bpa::routing::Error::Detaching(_) => {
            tonic::Status::already_exists(e.to_string())
        }
        hardy_bpa::routing::Error::Disconnected => tonic::Status::unavailable(e.to_string()),
        other => tonic::Status::internal(other.to_string()),
    }
}

// The BPA's end of a remote routing agent: it holds the sink the far
// agent's routes travel through, and reaches that agent through the
// proxy.
//
// `RouteAction` is also a wire message of this module's proto, so the
// BPA's type of that name stays fully qualified.
struct Agent {
    sink: Arc<dyn Sink>,
    node_ids: Arc<NodeIds>,
    // The agent is built before the proxy, which needs a handler holding
    // the agent, so the proxy arrives once the session is running.
    proxy: Once<RpcProxy<Result<BpaToAgent, tonic::Status>, AgentToBpa>>,
}

impl Agent {
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
impl hardy_bpa::routing::Agent for Agent {
    async fn shutdown(&self) -> hardy_bpa::routing::Result<()> {
        if let Some(proxy) = self.proxy.get() {
            proxy.shutdown().await;
        }
        Ok(())
    }
}

struct Handler {
    agent: Arc<Agent>,
}

#[async_trait]
impl ProxyHandler for Handler {
    type SMsg = bpa_to_agent::Msg;
    type RMsg = agent_to_bpa::Msg;

    async fn on_notify(&self, msg: Self::RMsg) -> Option<Self::SMsg> {
        let msg = match msg {
            agent_to_bpa::Msg::AddRoute(msg) => self.agent.add_route(msg).await,
            agent_to_bpa::Msg::RemoveRoute(msg) => self.agent.remove_route(msg).await,
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
        // The far agent is gone: detach, which withdraws its routes and
        // takes this agent down with the session. `cancel` rather than
        // `shutdown`, because the proxy task awaiting this handler is one
        // of the tasks being stopped.
        self.agent.sink.unregister().await;
        if let Some(proxy) = self.agent.proxy.get() {
            proxy.cancel();
        }
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
    mut channel_sender: tokio::sync::mpsc::Sender<Result<BpaToAgent, tonic::Status>>,
    mut channel_receiver: tonic::Streaming<AgentToBpa>,
    bpa: Arc<dyn hardy_bpa::routing::Registrar>,
) {
    // Filled by the registration handshake below, which the BPA drives
    // through the factory.
    let mut registered = None;

    // Wait for the client's registration message and process it
    let result = RpcProxy::recv(&mut channel_sender, &mut channel_receiver, |msg| async {
        match msg {
            agent_to_bpa::Msg::Register(request) => {
                let agent = bpa
                    .register_routing_agent(
                        &request.name,
                        factory_fn(|cx: Context| {
                            Ok(Arc::new(Agent {
                                sink: cx.sink,
                                node_ids: cx.node_ids,
                                proxy: Once::new(),
                            }))
                        }),
                    )
                    .await
                    .map_err(routing_error_to_status)?;

                let response = RegisterRoutingAgentResponse {
                    node_ids: Vec::<NodeId>::from(agent.node_ids.as_ref())
                        .into_iter()
                        .map(|node_id| node_id.to_string())
                        .collect(),
                };
                registered = Some(agent);

                Ok(bpa_to_agent::Msg::Register(response))
            }
            _ => {
                warn!("Routing agent sent incorrect message: {msg:?}");
                Err(tonic::Status::internal(format!(
                    "Unexpected response: {msg:?}"
                )))
            }
        }
    })
    .await;

    if let Err(e) = result {
        warn!("Routing agent registration failed: {e}");
        return;
    }

    let Some(agent) = registered else {
        warn!("Routing agent registration completed without registering an agent");
        return;
    };

    // Start the proxy for ongoing communication
    let handler = Box::new(Handler {
        agent: agent.clone(),
    });
    agent
        .proxy
        .call_once(|| RpcProxy::run(channel_sender, channel_receiver, handler, Side::Server));
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

    // The agent as the BPA builds it, against a sink the test owns.
    fn agent(sink: Arc<dyn Sink>) -> Agent {
        Agent {
            sink,
            node_ids: Arc::new(NodeIds::default()),
            proxy: Once::new(),
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
        let agent = agent(sink.clone());

        let msg = agent
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
        let agent = agent(sink.clone());

        let msg = agent
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
        let agent = agent(sink.clone());

        let unparseable_pattern = agent
            .add_route(AddRouteRequest {
                pattern: "not an EID pattern".to_string(),
                action: Some(via("ipn:2.0")),
                priority: 0,
            })
            .await
            .expect_err("the pattern does not parse");
        assert_eq!(unparseable_pattern.code(), tonic::Code::InvalidArgument);

        let missing_action = agent
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
