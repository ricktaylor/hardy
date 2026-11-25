// `Sink` is this module's own proxy sink, so the BPA's trait of that
// name is imported anonymously, for its methods alone.
use hardy_bpa::{
    node_ids::NodeIds,
    routing::{Agent, BoxFactory, Context, Sink as _},
};

use super::*;
use proto::routing::*;

struct Sink {
    proxy: RpcProxy<AgentToBpa, BpaToAgent>,
}

impl Sink {
    async fn call(&self, msg: agent_to_bpa::Msg) -> hardy_bpa::routing::Result<bpa_to_agent::Msg> {
        match self.proxy.call(msg).await {
            Err(e) => Err(hardy_bpa::routing::Error::Internal(e.into())),
            Ok(None) => Err(hardy_bpa::routing::Error::Disconnected),
            Ok(Some(msg)) => Ok(msg),
        }
    }
}

#[async_trait]
impl hardy_bpa::routing::Sink for Sink {
    async fn add_route(
        &self,
        pattern: hardy_eid_patterns::EidPattern,
        action: hardy_bpa::routing::RouteAction,
        priority: u32,
    ) -> hardy_bpa::routing::Result<bool> {
        match self
            .call(agent_to_bpa::Msg::AddRoute(AddRouteRequest {
                pattern: pattern.to_string(),
                action: Some((&action).into()),
                priority,
            }))
            .await?
        {
            bpa_to_agent::Msg::AddRoute(response) => Ok(response.added),
            msg => Err(hardy_bpa::routing::Error::Internal(
                tonic::Status::internal(format!("Unexpected response: {msg:?}")).into(),
            )),
        }
    }

    async fn remove_route(
        &self,
        pattern: &hardy_eid_patterns::EidPattern,
        action: &hardy_bpa::routing::RouteAction,
        priority: u32,
    ) -> hardy_bpa::routing::Result<bool> {
        match self
            .call(agent_to_bpa::Msg::RemoveRoute(RemoveRouteRequest {
                pattern: pattern.to_string(),
                action: Some(action.into()),
                priority,
            }))
            .await?
        {
            bpa_to_agent::Msg::RemoveRoute(response) => Ok(response.removed),
            msg => Err(hardy_bpa::routing::Error::Internal(
                tonic::Status::internal(format!("Unexpected response: {msg:?}")).into(),
            )),
        }
    }

    async fn unregister(&self) {
        self.proxy.shutdown().await;
    }
}

// The agent the session owns, shared with the proxy's handler.
//
// The agent is built from a context carrying the sink the proxy stands
// behind, so the proxy exists first and learns the agent it serves
// here. The handler holds the only strong reference: the session owns
// the agent it registered, and drops it on close.
type SessionAgent = Arc<Mutex<Option<Arc<dyn Agent>>>>;

struct Handler {
    agent: SessionAgent,
}

#[async_trait]
impl ProxyHandler for Handler {
    type SMsg = agent_to_bpa::Msg;
    type RMsg = bpa_to_agent::Msg;

    async fn on_notify(&self, msg: Self::RMsg) -> Option<Self::SMsg> {
        // Routing agents are push-only: the BPA never asks one for work.
        warn!("Ignoring unsolicited response: {msg:?}");
        None
    }

    async fn on_close(&self) {
        let agent = self.agent.lock().take();
        if let Some(agent) = agent
            && let Err(e) = agent.unregister().await
        {
            warn!("Routing agent unregister failed: {e}");
        }
    }
}

/// Registers a routing agent with the BPA over gRPC.
///
/// The bidirectional stream to the BPA's RoutingAgent endpoint stays open
/// for the agent's lifetime, and closing it unregisters the agent.
pub async fn register_routing_agent(
    grpc_addr: String,
    name: &str,
    factory: BoxFactory<'_>,
) -> hardy_bpa::routing::Result<()> {
    let mut client = routing_agent_client::RoutingAgentClient::connect(grpc_addr.clone())
        .await
        .map_err(|e| {
            error!("Failed to connect to gRPC server '{grpc_addr}': {e}");
            hardy_bpa::routing::Error::Internal(e.into())
        })?
        .max_encoding_message_size(crate::MAX_MESSAGE_SIZE)
        .max_decoding_message_size(crate::MAX_MESSAGE_SIZE);

    let (mut channel_sender, rx) = tokio::sync::mpsc::channel(16);

    let mut channel_receiver = client
        .register(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .map_err(|e| {
            error!("Routing agent registration failed: {e}");
            hardy_bpa::routing::Error::Internal(e.into())
        })?
        .into_inner();

    let response = match RpcProxy::send(
        &mut channel_sender,
        &mut channel_receiver,
        agent_to_bpa::Msg::Register(RegisterRoutingAgentRequest {
            name: name.to_string(),
        }),
    )
    .await
    .map_err(|e| {
        error!("Failed to send registration: {e}");
        hardy_bpa::routing::Error::Internal(e.into())
    })? {
        None => return Err(hardy_bpa::routing::Error::Disconnected),
        Some(bpa_to_agent::Msg::Register(response)) => response,
        Some(msg) => {
            error!("Routing agent registration failed: Unexpected response: {msg:?}");
            return Err(hardy_bpa::routing::Error::Internal(
                tonic::Status::internal(format!("Unexpected response: {msg:?}")).into(),
            ));
        }
    };

    let node_id_count = response.node_ids.len();
    let node_ids = response
        .node_ids
        .into_iter()
        .try_fold(Vec::with_capacity(node_id_count), |mut v, node_id| {
            v.push(node_id.parse::<hardy_bpv7::eid::NodeId>()?);
            Ok::<_, hardy_bpv7::eid::Error>(v)
        })
        .map_err(|e| {
            error!("Failed to parse node IDs in response: {e}");
            hardy_bpa::routing::Error::Internal(e.into())
        })?;
    let node_ids = NodeIds::try_from(node_ids.as_slice()).map_err(|e| {
        error!("Far BPA reported unusable node IDs: {e}");
        hardy_bpa::routing::Error::Internal(e.into())
    })?;

    let session: SessionAgent = Arc::new(Mutex::new(None));
    let proxy = RpcProxy::run(
        channel_sender,
        channel_receiver,
        Box::new(Handler {
            agent: session.clone(),
        }),
        Side::Client,
    );
    let sink = Arc::new(Sink { proxy });

    // The agent runs against the proxy from here, so a build or
    // registration failure takes the session down with it.
    let registered = async {
        let agent = factory.build(Context::new(sink.clone(), Arc::new(node_ids)))?;
        agent.register().await?;
        Ok(agent)
    }
    .await;

    match registered {
        Ok(agent) => {
            *session.lock() = Some(agent);
            info!("Proxy routing agent {name} registered");
            Ok(())
        }
        Err(e) => {
            sink.unregister().await;
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::ReceiverStream;

    // ── Mock routing agent ───────────────────────────────────────────

    // The lifecycle the session drives, observable from outside the
    // agent the factory builds.
    #[derive(Default)]
    struct Lifecycle {
        registered: AtomicBool,
        unregisters: AtomicUsize,
        unregistered: tokio::sync::Notify,
    }

    impl Lifecycle {
        fn is_registered(&self) -> bool {
            self.registered.load(Ordering::Relaxed)
        }

        fn is_unregistered(&self) -> bool {
            self.unregisters.load(Ordering::Relaxed) > 0
        }

        // Resolves once the agent has been unregistered at least once.
        async fn unregistered(&self) {
            let notified = self.unregistered.notified();
            if self.is_unregistered() {
                return;
            }
            notified.await;
        }
    }

    struct MockRoutingAgent {
        lifecycle: Arc<Lifecycle>,
        // A real agent keeps the sink for its whole active life, and the
        // session's proxy stands behind it: dropping it here would close
        // the stream under the test.
        _sink: Arc<dyn hardy_bpa::routing::Sink>,
    }

    impl MockRoutingAgent {
        fn factory(
            lifecycle: Arc<Lifecycle>,
        ) -> impl hardy_bpa::routing::Factory<Component = Self> {
            hardy_bpa::routing::factory_fn(move |cx: Context| {
                Ok(Arc::new(Self {
                    lifecycle,
                    _sink: cx.sink,
                }))
            })
        }
    }

    #[async_trait]
    impl Agent for MockRoutingAgent {
        async fn register(&self) -> hardy_bpa::routing::Result<()> {
            self.lifecycle.registered.store(true, Ordering::Relaxed);
            Ok(())
        }

        async fn unregister(&self) -> hardy_bpa::routing::Result<()> {
            self.lifecycle.unregisters.fetch_add(1, Ordering::Relaxed);
            self.lifecycle.unregistered.notify_waiters();
            Ok(())
        }
    }

    // ── Test port allocation ─────────────────────────────────────────

    fn test_port() -> u16 {
        static NEXT_PORT: AtomicUsize = AtomicUsize::new(50200);
        NEXT_PORT.fetch_add(1, Ordering::Relaxed) as u16
    }

    // The loopback host for test servers: IPv6 when available, IPv4 where
    // no ::1 is bindable.
    fn loopback_host() -> &'static str {
        static HOST: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
        HOST.get_or_init(|| {
            if std::net::TcpListener::bind(("::1", 0)).is_ok() {
                "[::1]"
            } else {
                "127.0.0.1"
            }
        })
    }

    // ── Custom mock servers ──────────────────────────────────────────

    async fn start_bad_server<S>(service: S) -> (String, tokio::task::JoinHandle<()>)
    where
        S: routing_agent_server::RoutingAgent,
    {
        let port = test_port();
        let host = loopback_host();
        let addr: std::net::SocketAddr = format!("{host}:{port}").parse().unwrap();
        let grpc_addr = format!("http://{host}:{port}");

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

    // Mock server that closes the stream immediately after registration.
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

                // Dropping tx here closes the stream right after the
                // handshake.
            });

            Ok(tonic::Response::new(ReceiverStream::new(rx)))
        }
    }

    // Mock server that sends the registration response twice.
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

                // Let the client consume the first response before the
                // duplicate, then hold the stream open long enough for it to
                // consume the duplicate too.
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                let _ = tx.send(Ok(response)).await;
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            });

            Ok(tonic::Response::new(ReceiverStream::new(rx)))
        }
    }

    // Mock server that sends an out-of-sequence message during handshake.
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
                // The registration message must be consumed before the
                // response goes out.
                let _ = stream.message().await;

                // The handshake expects msg_id 0, so an AddRouteResponse at
                // msg_id 5 is out of sequence.
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

    // ERR-CLI-02: Client shuts its agent down when the server closes the
    // stream immediately after the registration handshake.
    //
    // A custom mock server sends a valid registration response then
    // immediately drops the channel, closing the stream. The client
    // proxy's reader detects the closure and shuts the agent down via
    // `on_close`.
    #[tokio::test]
    async fn err_cli_02_premature_stream_end() {
        let (grpc_addr, server_handle) = start_bad_server(PrematureCloseServer).await;

        let lifecycle = Arc::new(Lifecycle::default());

        let result = register_routing_agent(
            grpc_addr,
            "test-agent",
            BoxFactory::new(MockRoutingAgent::factory(lifecycle.clone())),
        )
        .await;

        // Registration should succeed (the response was valid)
        assert!(result.is_ok(), "registration should succeed");
        assert!(lifecycle.is_registered());

        // The timeout only bounds a regression
        tokio::time::timeout(std::time::Duration::from_secs(5), lifecycle.unregistered())
            .await
            .expect("agent should have been unregistered after the stream closed");

        server_handle.abort();
    }

    // ERR-CLI-03: Client handles duplicate registration response without panic.
    //
    // A custom mock server sends `RegisterRoutingAgentResponse` twice.
    // The first response completes the handshake normally. The second
    // arrives unsolicited: the reader finds no pending entry for msg_id 0
    // and dispatches it to `on_notify`, which logs a warning and ignores
    // it.
    #[tokio::test]
    async fn err_cli_03_duplicate_register_response() {
        let (grpc_addr, server_handle) = start_bad_server(DuplicateRegisterServer).await;

        let lifecycle = Arc::new(Lifecycle::default());

        let result = register_routing_agent(
            grpc_addr,
            "test-agent",
            BoxFactory::new(MockRoutingAgent::factory(lifecycle.clone())),
        )
        .await;

        // Registration should succeed (first response is valid)
        assert!(result.is_ok(), "registration should succeed");
        assert!(lifecycle.is_registered());

        // Wait for the duplicate to arrive and be processed.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // Reaching here without a panic is half the assertion; once the
        // server's stream closes, the client unregisters its agent.
        assert!(
            lifecycle.is_unregistered(),
            "agent should have been unregistered after the server stream closed"
        );

        server_handle.abort();
    }

    // ERR-CLI-04: Client returns error on out-of-sequence message during
    // handshake.
    //
    // A custom mock server sends an `AddRouteResponse` with msg_id 5
    // instead of the expected `RegisterRoutingAgentResponse` with msg_id 0.
    // `RpcProxy::send()` detects the wrong msg_id and returns
    // `Err(Status::aborted("Out of sequence response"))`.
    #[tokio::test]
    async fn err_cli_04_invalid_message_sequence() {
        let (grpc_addr, server_handle) = start_bad_server(OutOfSequenceServer).await;

        let lifecycle = Arc::new(Lifecycle::default());

        let result = register_routing_agent(
            grpc_addr,
            "test-agent",
            BoxFactory::new(MockRoutingAgent::factory(lifecycle.clone())),
        )
        .await;

        assert!(
            result.is_err(),
            "registration should fail with out-of-sequence response"
        );
        assert!(
            !lifecycle.is_registered(),
            "agent should not have been built or registered"
        );

        server_handle.abort();
    }
}
