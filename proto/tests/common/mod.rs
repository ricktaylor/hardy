//! Shared test infrastructure for proto component tests.

pub mod sinks;

use hardy_async::async_trait;
use hardy_bpa::bpa::BpaRegistration;
use hardy_bpa::{cla, routes, services};
use hardy_bpv7::eid::NodeId;
use sinks::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

// ── Mock BPA ──────────────────────────────────────────────────────────

/// A mock BPA that implements `BpaRegistration` for all component types.
///
/// Calls `on_register` with mock sinks and fixed node IDs.
/// Tracks the last registered routing agent and sink for assertions.
pub struct MockBpa {
    node_ids: Vec<NodeId>,
    pub last_routing_sink: hardy_async::sync::spin::Mutex<Option<Arc<MockRoutingSink>>>,
    pub last_routing_agent: hardy_async::sync::spin::Mutex<Option<Arc<dyn routes::RoutingAgent>>>,
}

impl MockBpa {
    pub fn new() -> Self {
        Self {
            node_ids: vec!["ipn:1.0".parse().unwrap()],
            last_routing_sink: hardy_async::sync::spin::Mutex::new(None),
            last_routing_agent: hardy_async::sync::spin::Mutex::new(None),
        }
    }

    /// Simulate a server crash by forcing unregistration of all
    /// registered agents. This closes the proxy and stream without
    /// the client initiating it.
    pub async fn crash(&self) {
        if let Some(agent) = self.last_routing_agent.lock().take() {
            agent.on_unregister().await;
        }
    }
}

#[async_trait]
impl BpaRegistration for MockBpa {
    async fn register_cla(
        &self,
        _name: String,
        _address_type: Option<cla::ClaAddressType>,
        cla: Arc<dyn cla::Cla>,
        _policy: Option<Arc<dyn hardy_bpa::policy::EgressPolicy>>,
    ) -> cla::Result<Vec<NodeId>> {
        let sink = Arc::new(MockClaSink::new());
        cla.on_register(Box::new(ClaSinkWrapper(sink)), &self.node_ids)
            .await;
        Ok(self.node_ids.clone())
    }

    async fn register_service(
        &self,
        _service_id: Option<hardy_bpv7::eid::Service>,
        service: Arc<dyn services::Service>,
    ) -> services::Result<hardy_bpv7::eid::Eid> {
        let endpoint: hardy_bpv7::eid::Eid = "ipn:1.42".parse().unwrap();
        let sink = Arc::new(MockServiceSink::new());
        service
            .on_register(&endpoint, Box::new(ServiceSinkWrapper(sink)))
            .await;
        Ok(endpoint)
    }

    async fn register_application(
        &self,
        _service_id: Option<hardy_bpv7::eid::Service>,
        application: Arc<dyn services::Application>,
    ) -> services::Result<hardy_bpv7::eid::Eid> {
        let endpoint: hardy_bpv7::eid::Eid = "ipn:1.42".parse().unwrap();
        let sink = Arc::new(MockApplicationSink::new());
        application
            .on_register(&endpoint, Box::new(ApplicationSinkWrapper(sink)))
            .await;
        Ok(endpoint)
    }

    async fn register_routing_agent(
        &self,
        _name: String,
        agent: Arc<dyn routes::RoutingAgent>,
    ) -> routes::Result<Vec<NodeId>> {
        let sink = Arc::new(MockRoutingSink::new());
        *self.last_routing_sink.lock() = Some(sink.clone());
        *self.last_routing_agent.lock() = Some(agent.clone());

        agent
            .on_register(Box::new(RoutingSinkWrapper(sink)), &self.node_ids)
            .await;

        Ok(self.node_ids.clone())
    }
}

// ── Sink wrappers (delegate to Arc<Mock>) ─────────────────────────────

struct RoutingSinkWrapper(Arc<MockRoutingSink>);
struct ClaSinkWrapper(Arc<MockClaSink>);
struct ServiceSinkWrapper(Arc<MockServiceSink>);
struct ApplicationSinkWrapper(Arc<MockApplicationSink>);

#[async_trait]
impl routes::RoutingSink for RoutingSinkWrapper {
    async fn unregister(&self) {
        self.0.unregister().await;
    }
    async fn add_route(
        &self,
        p: hardy_eid_patterns::EidPattern,
        a: routes::Action,
        pri: u32,
    ) -> routes::Result<bool> {
        self.0.add_route(p, a, pri).await
    }
    async fn remove_route(
        &self,
        p: &hardy_eid_patterns::EidPattern,
        a: &routes::Action,
        pri: u32,
    ) -> routes::Result<bool> {
        self.0.remove_route(p, a, pri).await
    }
}

#[async_trait]
impl cla::Sink for ClaSinkWrapper {
    async fn unregister(&self) {
        self.0.unregister().await;
    }
    async fn dispatch(
        &self,
        b: hardy_bpa::Bytes,
        pn: Option<&NodeId>,
        pa: Option<&cla::ClaAddress>,
    ) -> cla::Result<()> {
        self.0.dispatch(b, pn, pa).await
    }
    async fn add_peer(&self, a: cla::ClaAddress, n: &[NodeId]) -> cla::Result<bool> {
        self.0.add_peer(a, n).await
    }
    async fn remove_peer(&self, a: &cla::ClaAddress) -> cla::Result<bool> {
        self.0.remove_peer(a).await
    }
}

#[async_trait]
impl services::ServiceSink for ServiceSinkWrapper {
    async fn unregister(&self) {
        self.0.unregister().await;
    }
    async fn send(&self, d: hardy_bpa::Bytes) -> services::Result<hardy_bpv7::bundle::Id> {
        self.0.send(d).await
    }
    async fn cancel(&self, id: &hardy_bpv7::bundle::Id) -> services::Result<bool> {
        self.0.cancel(id).await
    }
}

#[async_trait]
impl services::ApplicationSink for ApplicationSinkWrapper {
    async fn unregister(&self) {
        self.0.unregister().await;
    }
    async fn send(
        &self,
        dest: hardy_bpv7::eid::Eid,
        data: hardy_bpa::Bytes,
        lt: core::time::Duration,
        opts: Option<services::SendOptions>,
    ) -> services::Result<hardy_bpv7::bundle::Id> {
        self.0.send(dest, data, lt, opts).await
    }
    async fn cancel(&self, id: &hardy_bpv7::bundle::Id) -> services::Result<bool> {
        self.0.cancel(id).await
    }
}

// ── Server helpers ────────────────────────────────────────────────────

/// Allocate a unique port for each test to avoid conflicts.
fn test_port() -> u16 {
    static NEXT_PORT: AtomicUsize = AtomicUsize::new(50100);
    NEXT_PORT.fetch_add(1, Ordering::Relaxed) as u16
}

/// Start a gRPC server with the specified services.
/// Returns the gRPC address string and the task pool (cancel to stop).
pub async fn start_server(
    bpa: &Arc<MockBpa>,
    service_names: &[&str],
) -> (String, hardy_async::TaskPool) {
    let port = test_port();
    let addr: std::net::SocketAddr = format!("[::1]:{port}").parse().unwrap();
    let grpc_addr = format!("http://[::1]:{port}");

    let tasks = hardy_async::TaskPool::new();
    let config = hardy_proto::server::Config {
        address: addr,
        services: service_names.iter().map(|s| s.to_string()).collect(),
    };

    hardy_proto::server::init(&config, &(bpa.clone() as Arc<dyn BpaRegistration>), &tasks);

    // Give the server a moment to bind
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    (grpc_addr, tasks)
}

/// Start a gRPC server with the routing agent service.
pub async fn start_routing_server(bpa: &Arc<MockBpa>) -> (String, hardy_async::TaskPool) {
    start_server(bpa, &["routing"]).await
}
