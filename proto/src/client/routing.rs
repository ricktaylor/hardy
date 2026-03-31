use super::*;
use proto::routing::*;

struct Sink {
    proxy: RpcProxy<AgentToBpa, BpaToAgent>,
}

impl Sink {
    async fn call(&self, msg: agent_to_bpa::Msg) -> hardy_bpa::routes::Result<bpa_to_agent::Msg> {
        match self.proxy.call(msg).await {
            Err(e) => Err(hardy_bpa::routes::Error::Internal(e.into())),
            Ok(None) => Err(hardy_bpa::routes::Error::Disconnected),
            Ok(Some(msg)) => Ok(msg),
        }
    }
}

#[async_trait]
impl hardy_bpa::routes::RoutingSink for Sink {
    async fn add_route(
        &self,
        pattern: hardy_eid_patterns::EidPattern,
        action: hardy_bpa::routes::Action,
        priority: u32,
    ) -> hardy_bpa::routes::Result<bool> {
        match self
            .call(agent_to_bpa::Msg::AddRoute(AddRouteRequest {
                pattern: pattern.to_string(),
                action: Some((&action).into()),
                priority,
            }))
            .await?
        {
            bpa_to_agent::Msg::AddRoute(response) => Ok(response.added),
            msg => Err(hardy_bpa::routes::Error::Internal(
                tonic::Status::internal(format!("Unexpected response: {msg:?}")).into(),
            )),
        }
    }

    async fn remove_route(
        &self,
        pattern: &hardy_eid_patterns::EidPattern,
        action: &hardy_bpa::routes::Action,
        priority: u32,
    ) -> hardy_bpa::routes::Result<bool> {
        match self
            .call(agent_to_bpa::Msg::RemoveRoute(RemoveRouteRequest {
                pattern: pattern.to_string(),
                action: Some(action.into()),
                priority,
            }))
            .await?
        {
            bpa_to_agent::Msg::RemoveRoute(response) => Ok(response.removed),
            msg => Err(hardy_bpa::routes::Error::Internal(
                tonic::Status::internal(format!("Unexpected response: {msg:?}")).into(),
            )),
        }
    }

    async fn unregister(&self) {
        // Just shut down the proxy. The stream close triggers on_close
        // on the server, which unregisters from the BPA.
        self.proxy.shutdown().await;
    }
}

struct Handler {
    agent: Weak<dyn hardy_bpa::routes::RoutingAgent>,
}

#[async_trait]
impl ProxyHandler for Handler {
    type SMsg = agent_to_bpa::Msg;
    type RMsg = bpa_to_agent::Msg;

    async fn on_notify(&self, msg: Self::RMsg) -> Option<Self::SMsg> {
        {
            warn!("Ignoring unsolicited response: {msg:?}");
            None
        }
    }

    async fn on_close(&self) {
        if let Some(agent) = self.agent.upgrade() {
            agent.on_unregister().await;
        }
    }
}

pub async fn register_routing_agent(
    grpc_addr: String,
    name: String,
    agent: Arc<dyn hardy_bpa::routes::RoutingAgent>,
) -> hardy_bpa::routes::Result<Vec<hardy_bpv7::eid::NodeId>> {
    let mut client = routing_agent_client::RoutingAgentClient::connect(grpc_addr.clone())
        .await
        .map_err(|e| {
            error!("Failed to connect to gRPC server '{grpc_addr}': {e}");
            hardy_bpa::routes::Error::Internal(e.into())
        })?;

    let (mut channel_sender, rx) = tokio::sync::mpsc::channel(16);

    let mut channel_receiver = client
        .register(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .map_err(|e| {
            error!("Routing agent registration failed: {e}");
            hardy_bpa::routes::Error::Internal(e.into())
        })?
        .into_inner();

    // Send the initial registration message.
    let response = match RpcProxy::send(
        &mut channel_sender,
        &mut channel_receiver,
        agent_to_bpa::Msg::Register(RegisterRoutingAgentRequest { name: name.clone() }),
    )
    .await
    .map_err(|e| {
        error!("Failed to send registration: {e}");
        hardy_bpa::routes::Error::Internal(e.into())
    })? {
        None => return Err(hardy_bpa::routes::Error::Disconnected),
        Some(bpa_to_agent::Msg::Register(response)) => response,
        Some(msg) => {
            error!("Routing agent registration failed: Unexpected response: {msg:?}");
            return Err(hardy_bpa::routes::Error::Internal(
                tonic::Status::internal(format!("Unexpected response: {msg:?}")).into(),
            ));
        }
    };

    let node_ids = response
        .node_ids
        .into_iter()
        .try_fold(Vec::new(), |mut v, node_id| {
            v.push(node_id.parse::<hardy_bpv7::eid::NodeId>()?);
            Ok::<_, hardy_bpv7::eid::Error>(v)
        })
        .map_err(|e| {
            error!("Failed to parse node IDs in response: {e}");
            hardy_bpa::routes::Error::Internal(e.into())
        })?;

    let handler = Box::new(Handler {
        agent: Arc::downgrade(&agent),
    });

    // Start the proxy
    let proxy = RpcProxy::run(channel_sender, channel_receiver, handler);

    // Call on_register()
    agent
        .on_register(Box::new(Sink { proxy }), node_ids.as_slice())
        .await;

    info!("Proxy routing agent {name} started");
    Ok(node_ids)
}
