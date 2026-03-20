use super::*;
use proto::routing::*;

struct RoutingAgentInner {
    sink: Box<dyn hardy_bpa::routes::RoutingSink>,
}

struct RemoteRoutingAgent {
    inner: Once<RoutingAgentInner>,
    proxy: Once<RpcProxy<Result<BpaToAgent, tonic::Status>, AgentToBpa>>,
}

impl RemoteRoutingAgent {
    async fn call(&self, msg: bpa_to_agent::Msg) -> hardy_bpa::routes::Result<agent_to_bpa::Msg> {
        let proxy = self.proxy.get().ok_or_else(|| {
            error!("call made before on_register!");
            hardy_bpa::routes::Error::Disconnected
        })?;

        match proxy.call(msg).await {
            Ok(None) => Err(hardy_bpa::routes::Error::Disconnected),
            Ok(Some(msg)) => Ok(msg),
            Err(e) => Err(hardy_bpa::routes::Error::Internal(e.into())),
        }
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
            .inner
            .get()
            .ok_or(tonic::Status::internal("on_register not called"))?
            .sink
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
            .inner
            .get()
            .ok_or(tonic::Status::internal("on_register not called"))?
            .sink
            .remove_route(&pattern, &action, request.priority)
            .await
            .map_err(|e| tonic::Status::from_error(e.into()))?;

        Ok(bpa_to_agent::Msg::RemoveRoute(RemoveRouteResponse {
            removed,
        }))
    }

    async fn unregister(&self) {
        if let Some(inner) = self.inner.get() {
            inner.sink.unregister().await
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
        self.inner.call_once(|| RoutingAgentInner { sink });
    }

    async fn on_unregister(&self) {
        match self
            .call(bpa_to_agent::Msg::OnUnregister(
                OnUnregisterRoutingAgentRequest {},
            ))
            .await
        {
            Ok(agent_to_bpa::Msg::OnUnregister(_)) => {}
            Ok(msg) => {
                warn!("Unexpected response: {msg:?}");
            }
            Err(e) => {
                warn!("Failed to notify routing agent of unregistration: {e}");
            }
        }

        if let Some(proxy) = self.proxy.get() {
            proxy.close().await;
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
            agent_to_bpa::Msg::Unregister(_) => {
                self.agent.unregister().await;
                Ok(bpa_to_agent::Msg::Unregister(
                    UnregisterRoutingAgentResponse {},
                ))
            }
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
        // Do nothing
    }
}

pub struct Service {
    bpa: Arc<dyn hardy_bpa::bpa::BpaRegistration>,
    channel_size: usize,
}

#[async_trait]
impl routing_agent_server::RoutingAgent for Service {
    type RegisterStream = tokio_stream::wrappers::ReceiverStream<Result<BpaToAgent, tonic::Status>>;

    async fn register(
        &self,
        request: tonic::Request<tonic::Streaming<AgentToBpa>>,
    ) -> Result<tonic::Response<Self::RegisterStream>, tonic::Status> {
        let (mut channel_sender, rx) = tokio::sync::mpsc::channel(self.channel_size);
        let mut channel_receiver = request.into_inner();

        let agent = Arc::new(RemoteRoutingAgent {
            inner: Once::new(),
            proxy: Once::new(),
        });

        RpcProxy::recv(&mut channel_sender, &mut channel_receiver, |msg| async {
            match msg {
                agent_to_bpa::Msg::Register(request) => {
                    let node_ids = self
                        .bpa
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
        .await?;

        // Start the proxy
        let handler = Box::new(Handler {
            agent: agent.clone(),
        });
        agent
            .proxy
            .call_once(|| RpcProxy::run(channel_sender, channel_receiver, handler));

        Ok(tonic::Response::new(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ))
    }
}

/// Create a new RoutingAgent gRPC service.
pub fn new_routing_agent_service(
    bpa: &Arc<dyn hardy_bpa::bpa::BpaRegistration>,
) -> routing_agent_server::RoutingAgentServer<Service> {
    routing_agent_server::RoutingAgentServer::new(Service {
        bpa: bpa.clone(),
        channel_size: 16,
    })
}
