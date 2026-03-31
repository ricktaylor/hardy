use super::*;
use proto::routing::*;

type RoutingSink = Arc<dyn hardy_bpa::routes::RoutingSink>;

struct RemoteRoutingAgent {
    sink: Mutex<Option<RoutingSink>>,
    proxy: Once<RpcProxy<Result<BpaToAgent, tonic::Status>, AgentToBpa>>,
}

impl RemoteRoutingAgent {
    fn sink(&self) -> Result<RoutingSink, tonic::Status> {
        self.sink
            .lock()
            .clone()
            .ok_or(tonic::Status::unavailable("Unregistered"))
    }

    async fn add_route(
        &self,
        request: AddRouteRequest,
    ) -> Result<bpa_to_agent::Msg, tonic::Status> {
        let pattern = request
            .pattern
            .parse()
            .map_err(|e| tonic::Status::invalid_argument(format!("Invalid EID pattern: {e}")))?;

        let action: hardy_bpa::routes::Action = request
            .action
            .ok_or(tonic::Status::invalid_argument("Missing action"))?
            .try_into()?;

        let added = self
            .sink()?
            .add_route(pattern, action, request.priority)
            .await
            .map_err(|e| tonic::Status::from_error(e.into()))?;

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

        let action: hardy_bpa::routes::Action = request
            .action
            .ok_or(tonic::Status::invalid_argument("Missing action"))?
            .try_into()?;

        let removed = self
            .sink()?
            .remove_route(&pattern, &action, request.priority)
            .await
            .map_err(|e| tonic::Status::from_error(e.into()))?;

        Ok(bpa_to_agent::Msg::RemoveRoute(RemoveRouteResponse {
            removed,
        }))
    }

    /// Take the sink and unregister from the BPA. No-op if already taken.
    async fn unregister(&self) {
        let sink = self.sink.lock().take();
        if let Some(sink) = sink {
            sink.unregister().await;
        }
    }
}

#[async_trait]
impl hardy_bpa::routes::RoutingAgent for RemoteRoutingAgent {
    async fn on_register(
        &self,
        sink: Box<dyn hardy_bpa::routes::RoutingSink>,
        _node_ids: &[hardy_bpv7::eid::NodeId],
    ) {
        *self.sink.lock() = Some(Arc::from(sink));
    }

    async fn on_unregister(&self) {
        if self.sink.lock().take().is_none() {
            return;
        }

        if let Some(proxy) = self.proxy.get() {
            proxy.shutdown().await;
        }
    }
}

struct Handler {
    agent: Arc<RemoteRoutingAgent>,
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
        self.agent.unregister().await;
        if let Some(proxy) = self.agent.proxy.get() {
            proxy.on_unregister();
        }
    }
}

pub struct Service {
    bpa: Arc<dyn hardy_bpa::bpa::BpaRegistration>,
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
    bpa: Arc<dyn hardy_bpa::bpa::BpaRegistration>,
) {
    let agent = Arc::new(RemoteRoutingAgent {
        sink: Mutex::new(None),
        proxy: Once::new(),
    });

    // Wait for the client's registration message and process it
    let result = RpcProxy::recv(&mut channel_sender, &mut channel_receiver, |msg| async {
        match msg {
            agent_to_bpa::Msg::Register(request) => {
                let node_ids = bpa
                    .register_routing_agent(request.name, agent.clone())
                    .await
                    .map_err(|e| tonic::Status::from_error(e.into()))?
                    .into_iter()
                    .map(|node_id| node_id.to_string())
                    .collect();

                Ok(bpa_to_agent::Msg::Register(RegisterRoutingAgentResponse {
                    node_ids,
                }))
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

    // Start the proxy for ongoing communication
    let handler = Box::new(Handler {
        agent: agent.clone(),
    });
    agent
        .proxy
        .call_once(|| RpcProxy::run(channel_sender, channel_receiver, handler));
}

/// Create a new RoutingAgent gRPC service.
pub fn new_routing_agent_service(
    bpa: &Arc<dyn hardy_bpa::bpa::BpaRegistration>,
    tasks: &hardy_async::TaskPool,
) -> routing_agent_server::RoutingAgentServer<Service> {
    routing_agent_server::RoutingAgentServer::new(Service {
        bpa: bpa.clone(),
        session_tasks: tasks.clone(),
        channel_size: 16,
    })
}
