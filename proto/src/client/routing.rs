use std::sync::{Arc, Weak};

use hardy_async::{CancellationToken, async_trait};
use hardy_bpa::routes::{self, RouteOp, RoutingAgent, RoutingContext};
use hardy_bpv7::eid::NodeId;
use tracing::{error, info, warn};

use crate::proto::routing::{
    AddRouteRequest, RegisterRoutingAgentRequest, RemoveRouteRequest, agent_to_bpa, bpa_to_agent,
    routing_agent_client,
};
use crate::proxy::{ProxyHandler, RpcProxy};

struct Handler {
    agent: Weak<dyn RoutingAgent>,
    shutdown: CancellationToken,
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
        self.shutdown.cancel();
        if let Some(agent) = self.agent.upgrade() {
            agent.on_unregister().await;
        }
    }
}

pub async fn register_routing_agent(
    grpc_addr: String,
    name: String,
    agent: Arc<dyn RoutingAgent>,
) -> routes::Result<Vec<NodeId>> {
    let mut client = routing_agent_client::RoutingAgentClient::connect(grpc_addr.clone())
        .await
        .map_err(|e| {
            error!("Failed to connect to gRPC server '{grpc_addr}': {e}");
            routes::Error::Internal(e.into())
        })?;

    let (mut channel_sender, rx) = tokio::sync::mpsc::channel(16);

    let mut channel_receiver = client
        .register(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .map_err(|e| {
            error!("Routing agent registration failed: {e}");
            routes::Error::Internal(e.into())
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
        routes::Error::Internal(e.into())
    })? {
        None => return Err(routes::Error::Disconnected),
        Some(bpa_to_agent::Msg::Register(response)) => response,
        Some(msg) => {
            error!("Routing agent registration failed: Unexpected response: {msg:?}");
            return Err(routes::Error::Internal(
                tonic::Status::internal(format!("Unexpected response: {msg:?}")).into(),
            ));
        }
    };

    let node_ids = response
        .node_ids
        .into_iter()
        .try_fold(Vec::new(), |mut v, node_id| {
            v.push(node_id.parse::<NodeId>()?);
            Ok::<_, hardy_bpv7::eid::Error>(v)
        })
        .map_err(|e| {
            error!("Failed to parse node IDs in response: {e}");
            routes::Error::Internal(e.into())
        })?;

    let (route_tx, route_rx) = flume::unbounded();
    let shutdown = hardy_async::CancellationToken::new();
    let ctx = RoutingContext::new(route_tx, shutdown.clone());

    let handler = Box::new(Handler {
        agent: Arc::downgrade(&agent),
        shutdown,
    });

    let proxy = Arc::new(RpcProxy::run(channel_sender, channel_receiver, handler));

    // Spawn a task that reads RouteOps and sends them as gRPC messages
    let proxy_clone = proxy.clone();
    tokio::spawn(async move {
        while let Ok(op) = route_rx.recv_async().await {
            let msg = match op {
                RouteOp::Add {
                    pattern,
                    action,
                    priority,
                } => agent_to_bpa::Msg::AddRoute(AddRouteRequest {
                    pattern: pattern.to_string(),
                    action: Some((&action).into()),
                    priority,
                }),
                RouteOp::Remove {
                    pattern,
                    action,
                    priority,
                } => agent_to_bpa::Msg::RemoveRoute(RemoveRouteRequest {
                    pattern: pattern.to_string(),
                    action: Some((&action).into()),
                    priority,
                }),
            };
            if proxy_clone.call(msg).await.is_err() {
                break;
            }
        }
    });

    // Call on_register with the context
    agent.on_register(ctx, node_ids.as_slice()).await;

    info!("Proxy routing agent {name} started");
    Ok(node_ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::routing::{
        AddRouteResponse, AgentToBpa, BpaToAgent, RegisterRoutingAgentResponse,
        routing_agent_server,
    };
    use hardy_async::sync::spin::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::ReceiverStream;

    struct MockRoutingAgent {
        registered: AtomicBool,
        unregister_count: AtomicUsize,
        ctx: Mutex<Option<RoutingContext>>,
    }

    impl MockRoutingAgent {
        fn new() -> Self {
            Self {
                registered: AtomicBool::new(false),
                unregister_count: AtomicUsize::new(0),
                ctx: Mutex::new(None),
            }
        }

        fn is_registered(&self) -> bool {
            self.registered.load(Ordering::Relaxed)
        }

        fn is_unregistered(&self) -> bool {
            self.unregister_count.load(Ordering::Relaxed) > 0
        }
    }

    #[async_trait]
    impl RoutingAgent for MockRoutingAgent {
        async fn on_register(&self, ctx: RoutingContext, _node_ids: &[NodeId]) {
            *self.ctx.lock() = Some(ctx);
            self.registered.store(true, Ordering::Relaxed);
        }

        async fn on_unregister(&self) {
            self.unregister_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn test_port() -> u16 {
        static NEXT_PORT: AtomicUsize = AtomicUsize::new(50200);
        NEXT_PORT.fetch_add(1, Ordering::Relaxed) as u16
    }

    async fn start_bad_server<S>(service: S) -> (String, tokio::task::JoinHandle<()>)
    where
        S: routing_agent_server::RoutingAgent,
    {
        let port = test_port();
        let addr: std::net::SocketAddr = format!("[::1]:{port}").parse().unwrap();
        let grpc_addr = format!("http://[::1]:{port}");

        let handle = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(routing_agent_server::RoutingAgentServer::new(service))
                .serve(addr)
                .await
                .unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        (grpc_addr, handle)
    }

    struct PrematureCloseServer;

    #[async_trait]
    impl routing_agent_server::RoutingAgent for PrematureCloseServer {
        type RegisterStream = ReceiverStream<Result<BpaToAgent, tonic::Status>>;

        async fn register(
            &self,
            request: tonic::Request<tonic::Streaming<AgentToBpa>>,
        ) -> Result<tonic::Response<Self::RegisterStream>, tonic::Status> {
            let (tx, rx) = mpsc::channel(16);
            let mut stream = request.into_inner();

            tokio::spawn(async move {
                let _ = stream.message().await;

                let _ = tx
                    .send(Ok(BpaToAgent {
                        msg_id: 0,
                        msg: Some(bpa_to_agent::Msg::Register(RegisterRoutingAgentResponse {
                            node_ids: vec!["ipn:1.0".to_string()],
                        })),
                    }))
                    .await;
            });

            Ok(tonic::Response::new(ReceiverStream::new(rx)))
        }
    }

    struct DuplicateRegisterServer;

    #[async_trait]
    impl routing_agent_server::RoutingAgent for DuplicateRegisterServer {
        type RegisterStream = ReceiverStream<Result<BpaToAgent, tonic::Status>>;

        async fn register(
            &self,
            request: tonic::Request<tonic::Streaming<AgentToBpa>>,
        ) -> Result<tonic::Response<Self::RegisterStream>, tonic::Status> {
            let (tx, rx) = mpsc::channel(16);
            let mut stream = request.into_inner();

            tokio::spawn(async move {
                let _ = stream.message().await;

                let response = BpaToAgent {
                    msg_id: 0,
                    msg: Some(bpa_to_agent::Msg::Register(RegisterRoutingAgentResponse {
                        node_ids: vec!["ipn:1.0".to_string()],
                    })),
                };

                let _ = tx.send(Ok(response.clone())).await;

                tokio::time::sleep(std::time::Duration::from_millis(50)).await;

                let _ = tx.send(Ok(response)).await;

                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            });

            Ok(tonic::Response::new(ReceiverStream::new(rx)))
        }
    }

    struct OutOfSequenceServer;

    #[async_trait]
    impl routing_agent_server::RoutingAgent for OutOfSequenceServer {
        type RegisterStream = ReceiverStream<Result<BpaToAgent, tonic::Status>>;

        async fn register(
            &self,
            request: tonic::Request<tonic::Streaming<AgentToBpa>>,
        ) -> Result<tonic::Response<Self::RegisterStream>, tonic::Status> {
            let (tx, rx) = mpsc::channel(16);
            let mut stream = request.into_inner();

            tokio::spawn(async move {
                let _ = stream.message().await;

                let _ = tx
                    .send(Ok(BpaToAgent {
                        msg_id: 5,
                        msg: Some(bpa_to_agent::Msg::AddRoute(AddRouteResponse {
                            added: true,
                        })),
                    }))
                    .await;
            });

            Ok(tonic::Response::new(ReceiverStream::new(rx)))
        }
    }

    #[tokio::test]
    async fn err_cli_02_premature_stream_end() {
        let (grpc_addr, server_handle) = start_bad_server(PrematureCloseServer).await;

        let agent = Arc::new(MockRoutingAgent::new());

        let result =
            register_routing_agent(grpc_addr, "test-agent".to_string(), agent.clone()).await;

        assert!(result.is_ok(), "registration should succeed");
        assert!(agent.is_registered());

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        assert!(
            agent.is_unregistered(),
            "agent should have received synthetic on_unregister after stream close"
        );

        server_handle.abort();
    }

    #[tokio::test]
    async fn err_cli_03_duplicate_register_response() {
        let (grpc_addr, server_handle) = start_bad_server(DuplicateRegisterServer).await;

        let agent = Arc::new(MockRoutingAgent::new());

        let result =
            register_routing_agent(grpc_addr, "test-agent".to_string(), agent.clone()).await;

        assert!(result.is_ok(), "registration should succeed");
        assert!(agent.is_registered());

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        assert!(
            agent.is_unregistered(),
            "agent should have received on_unregister after server stream closed"
        );

        server_handle.abort();
    }

    #[tokio::test]
    async fn err_cli_04_invalid_message_sequence() {
        let (grpc_addr, server_handle) = start_bad_server(OutOfSequenceServer).await;

        let agent = Arc::new(MockRoutingAgent::new());

        let result =
            register_routing_agent(grpc_addr, "test-agent".to_string(), agent.clone()).await;

        assert!(
            result.is_err(),
            "registration should fail with out-of-sequence response"
        );
        assert!(
            !agent.is_registered(),
            "agent should not have received on_register"
        );

        server_handle.abort();
    }
}
