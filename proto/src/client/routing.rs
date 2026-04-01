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

#[cfg(test)]
mod tests {
    use super::*;
    use hardy_bpa::routes::{RoutingAgent, RoutingSink};
    use hardy_bpv7::eid::NodeId;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::ReceiverStream;

    // ── Mock routing agent ───────────────────────────────────────────

    struct MockRoutingAgent {
        registered: AtomicBool,
        unregister_count: AtomicUsize,
        sink: Mutex<Option<Box<dyn RoutingSink>>>,
    }

    impl MockRoutingAgent {
        fn new() -> Self {
            Self {
                registered: AtomicBool::new(false),
                unregister_count: AtomicUsize::new(0),
                sink: Mutex::new(None),
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
        async fn on_register(&self, sink: Box<dyn RoutingSink>, _node_ids: &[NodeId]) {
            *self.sink.lock() = Some(sink);
            self.registered.store(true, Ordering::Relaxed);
        }

        async fn on_unregister(&self) {
            self.unregister_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    // ── Test port allocation ─────────────────────────────────────────

    fn test_port() -> u16 {
        static NEXT_PORT: AtomicUsize = AtomicUsize::new(50200);
        NEXT_PORT.fetch_add(1, Ordering::Relaxed) as u16
    }

    // ── Custom mock servers ──────────────────────────────────────────

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

        // Give the server a moment to bind
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        (grpc_addr, handle)
    }

    /// Mock server that closes the stream immediately after registration.
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
                // Read the client's registration message
                let _ = stream.message().await;

                // Send valid response
                let _ = tx
                    .send(Ok(BpaToAgent {
                        msg_id: 0,
                        msg: Some(bpa_to_agent::Msg::Register(RegisterRoutingAgentResponse {
                            node_ids: vec!["ipn:1.0".to_string()],
                        })),
                    }))
                    .await;

                // Drop tx immediately — closes the stream right after handshake
            });

            Ok(tonic::Response::new(ReceiverStream::new(rx)))
        }
    }

    /// Mock server that sends the registration response twice.
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
                // Read the client's registration message
                let _ = stream.message().await;

                let response = BpaToAgent {
                    msg_id: 0,
                    msg: Some(bpa_to_agent::Msg::Register(RegisterRoutingAgentResponse {
                        node_ids: vec!["ipn:1.0".to_string()],
                    })),
                };

                // Send valid response
                let _ = tx.send(Ok(response.clone())).await;

                // Small delay to ensure the first response is processed
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;

                // Send duplicate response (protocol violation)
                let _ = tx.send(Ok(response)).await;

                // Keep stream alive briefly so the client reader processes the duplicate
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            });

            Ok(tonic::Response::new(ReceiverStream::new(rx)))
        }
    }

    /// Mock server that sends an out-of-sequence message during handshake.
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
                // Read the client's registration message (must consume it)
                let _ = stream.message().await;

                // Send an AddRouteResponse with non-zero msg_id — the client
                // handshake expects msg_id 0.
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

    // ── Tests ────────────────────────────────────────────────────────

    /// ERR-CLI-02: Client receives synthetic on_unregister when server
    /// closes the stream immediately after the registration handshake.
    ///
    /// A custom mock server sends a valid registration response then
    /// immediately drops the channel, closing the stream. The client
    /// proxy's reader detects the closure and delivers a synthetic
    /// `on_unregister()` via `on_close`.
    #[tokio::test]
    async fn err_cli_02_premature_stream_end() {
        let (grpc_addr, server_handle) = start_bad_server(PrematureCloseServer).await;

        let agent = Arc::new(MockRoutingAgent::new());

        let result =
            register_routing_agent(grpc_addr, "test-agent".to_string(), agent.clone()).await;

        // Registration should succeed (the response was valid)
        assert!(result.is_ok(), "registration should succeed");
        assert!(agent.is_registered());

        // Give the client time to detect the stream close
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        assert!(
            agent.is_unregistered(),
            "agent should have received synthetic on_unregister after stream close"
        );

        server_handle.abort();
    }

    /// ERR-CLI-03: Client handles duplicate registration response without panic.
    ///
    /// A custom mock server sends `RegisterRoutingAgentResponse` twice.
    /// The first response completes the handshake normally. The second
    /// arrives as an unsolicited message — the reader finds no pending
    /// entry for msg_id 0 and dispatches it to `on_notify`, which logs
    /// a warning and ignores it.
    #[tokio::test]
    async fn err_cli_03_duplicate_register_response() {
        let (grpc_addr, server_handle) = start_bad_server(DuplicateRegisterServer).await;

        let agent = Arc::new(MockRoutingAgent::new());

        let result =
            register_routing_agent(grpc_addr, "test-agent".to_string(), agent.clone()).await;

        // Registration should succeed (first response is valid)
        assert!(result.is_ok(), "registration should succeed");
        assert!(agent.is_registered());

        // Wait for the duplicate to arrive and be processed
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // No panic — test completing is the primary assertion.
        // After the server's stream closes, the client should get on_unregister.
        assert!(
            agent.is_unregistered(),
            "agent should have received on_unregister after server stream closed"
        );

        server_handle.abort();
    }

    /// ERR-CLI-04: Client returns error on out-of-sequence message during
    /// handshake.
    ///
    /// A custom mock server sends an `AddRouteResponse` with msg_id 5
    /// instead of the expected `RegisterRoutingAgentResponse` with msg_id 0.
    /// `RpcProxy::send()` detects the wrong msg_id and returns
    /// `Err(Status::aborted("Out of sequence response"))`.
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
