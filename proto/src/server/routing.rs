use std::sync::Arc;

use hardy_async::async_trait;
use hardy_async::sync::spin::{Mutex, Once};
use hardy_bpa::bpa::BpaRegistration;
use hardy_bpa::routes::{Action, RoutingAgent, RoutingContext};
use tracing::warn;

use crate::proto::routing::{
    AddRouteRequest, AddRouteResponse, AgentToBpa, BpaToAgent, RegisterRoutingAgentResponse,
    RemoveRouteRequest, RemoveRouteResponse, agent_to_bpa, bpa_to_agent, routing_agent_server,
};
use crate::proxy::{ProxyHandler, RpcProxy};

struct RemoteRoutingAgent {
    ctx: Mutex<Option<RoutingContext>>,
    proxy: Once<RpcProxy<Result<BpaToAgent, tonic::Status>, AgentToBpa>>,
}

impl RemoteRoutingAgent {
    fn ctx(&self) -> Result<RoutingContext, tonic::Status> {
        self.ctx
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

        let action: Action = request
            .action
            .ok_or(tonic::Status::invalid_argument("Missing action"))?
            .try_into()?;

        self.ctx()?.add_route(pattern, action, request.priority);

        Ok(bpa_to_agent::Msg::AddRoute(AddRouteResponse {
            added: true,
        }))
    }

    async fn remove_route(
        &self,
        request: RemoveRouteRequest,
    ) -> Result<bpa_to_agent::Msg, tonic::Status> {
        let pattern = request
            .pattern
            .parse()
            .map_err(|e| tonic::Status::invalid_argument(format!("Invalid EID pattern: {e}")))?;

        let action: Action = request
            .action
            .ok_or(tonic::Status::invalid_argument("Missing action"))?
            .try_into()?;

        self.ctx()?
            .remove_route(&pattern, &action, request.priority);

        Ok(bpa_to_agent::Msg::RemoveRoute(RemoveRouteResponse {
            removed: true,
        }))
    }

    fn disconnect(&self) {
        self.ctx.lock().take();
    }
}

#[async_trait]
impl RoutingAgent for RemoteRoutingAgent {
    async fn on_register(&self, ctx: RoutingContext, _node_ids: &[hardy_bpv7::eid::NodeId]) {
        *self.ctx.lock() = Some(ctx);
    }

    async fn on_unregister(&self) {
        if self.ctx.lock().take().is_none() {
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
        self.agent.disconnect();
        if let Some(proxy) = self.agent.proxy.get() {
            proxy.cancel();
        }
    }
}

pub struct Service {
    bpa: Arc<dyn BpaRegistration>,
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
    bpa: Arc<dyn BpaRegistration>,
) {
    let agent = Arc::new(RemoteRoutingAgent {
        ctx: Mutex::new(None),
        proxy: Once::new(),
    });

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

    let handler = Box::new(Handler {
        agent: agent.clone(),
    });
    agent
        .proxy
        .call_once(|| RpcProxy::run(channel_sender, channel_receiver, handler));
}

/// Create a new RoutingAgent gRPC service.
pub fn new_routing_agent_service(
    bpa: &Arc<dyn BpaRegistration>,
    tasks: &hardy_async::TaskPool,
) -> routing_agent_server::RoutingAgentServer<Service> {
    routing_agent_server::RoutingAgentServer::new(Service {
        bpa: bpa.clone(),
        session_tasks: tasks.clone(),
        channel_size: 16,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // SRV-02: After registration stores a context, `ctx()` returns it.
    #[test]
    fn srv_02_ctx_available_after_register() {
        let agent = RemoteRoutingAgent {
            ctx: Mutex::new(None),
            proxy: Once::new(),
        };

        assert!(agent.ctx().is_err());

        let (tx, _rx) = flume::unbounded();
        let token = hardy_async::CancellationToken::new();
        *agent.ctx.lock() = Some(RoutingContext::new(tx, token));

        assert!(agent.ctx().is_ok());
    }

    // SRV-03: After the context is taken (unregistration), `ctx()` returns
    // `Err(Unavailable)`.
    #[test]
    fn srv_03_ctx_unavailable_after_unregister() {
        let (tx, _rx) = flume::unbounded();
        let token = hardy_async::CancellationToken::new();
        let agent = RemoteRoutingAgent {
            ctx: Mutex::new(Some(RoutingContext::new(tx, token))),
            proxy: Once::new(),
        };

        assert!(agent.ctx().is_ok());

        agent.ctx.lock().take();

        let err = agent.ctx().err().expect("ctx() should return Err");
        assert_eq!(err.code(), tonic::Code::Unavailable);
    }

    // SRV-04: `disconnect()` does not deadlock (no async await while holding lock).
    #[test]
    fn srv_04_disconnect_no_deadlock() {
        let (tx, _rx) = flume::unbounded();
        let token = hardy_async::CancellationToken::new();
        let agent = RemoteRoutingAgent {
            ctx: Mutex::new(Some(RoutingContext::new(tx, token))),
            proxy: Once::new(),
        };

        agent.disconnect();

        assert!(agent.ctx().is_err());
    }
}
