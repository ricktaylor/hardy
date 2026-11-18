//! Shared test infrastructure for proto component tests.
//!
//! Each integration test binary compiles this module independently,
//! so items used by other test files appear unused in each binary.
#![allow(dead_code)]

pub mod sinks;

use core::num::NonZeroU64;
use hardy_async::async_trait;
use hardy_async::sync::spin::Mutex;
use hardy_bpa::bpa::BpaRegistration;
use hardy_bpa::node_ids::NodeIds;
use hardy_bpa::{
    cla, routing,
    service::{self, SendOptions, Service, Sink, raw},
};
// `eid::Service` is the service number inside an EID, not the service
// trait registered here; it is aliased to the word the code already uses
// for it.
use hardy_bpv7::eid::{Eid, NodeId, Service as ServiceId};
use sinks::*;
use std::sync::Arc;

// ── Mock BPA ──────────────────────────────────────────────────────────

/// A mock BPA that implements `BpaRegistration` for all component types.
///
/// Builds each component with a mock sink and fixed node IDs, and tracks
/// the last of each kind, and its sink, for assertions.
pub struct MockBpa {
    node_ids: Arc<NodeIds>,
    /// This BPA's own receive limit, folded into the terms it settles.
    max_reception_size: Option<NonZeroU64>,
    /// What the last `register_cla_boxed` asked for.
    pub declared_request: Mutex<Option<cla::Request>>,
    /// The terms the last `register_cla_boxed` settled on.
    pub settled_terms: Mutex<Option<cla::Terms>>,
    pub last_routing_sink: Mutex<Option<Arc<MockRoutingSink>>>,
    pub last_routing_agent: Mutex<Option<Arc<dyn routing::Agent>>>,
    pub last_cla: Mutex<Option<Arc<dyn cla::Cla>>>,
    pub last_cla_sink: Mutex<Option<Arc<MockClaSink>>>,
    pub last_raw_service: Mutex<Option<Arc<dyn raw::Service>>>,
    pub last_service: Mutex<Option<Arc<dyn Service>>>,
}

impl MockBpa {
    pub fn new() -> Self {
        let node_ids = NodeIds::try_from(["ipn:1.0".parse::<NodeId>().unwrap()].as_slice())
            .expect("ipn:1.0 is a valid node identifier");
        Self {
            node_ids: Arc::new(node_ids),
            max_reception_size: None,
            declared_request: Mutex::new(None),
            settled_terms: Mutex::new(None),
            last_routing_sink: Mutex::new(None),
            last_routing_agent: Mutex::new(None),
            last_cla: Mutex::new(None),
            last_cla_sink: Mutex::new(None),
            last_raw_service: Mutex::new(None),
            last_service: Mutex::new(None),
        }
    }

    /// A mock whose own receive limit is `octets`, so a CLA declaring a
    /// larger one is settled down to it.
    pub fn with_max_reception_size(octets: NonZeroU64) -> Self {
        Self {
            max_reception_size: Some(octets),
            ..Self::new()
        }
    }

    // Settles a request as the BPA does: the smaller receive limit wins,
    // a side that declared none defers to the other, and the lane count
    // passes through as asked.
    fn negotiate(&self, request: &cla::Request) -> cla::Terms {
        cla::Terms {
            lane_count: request.lane_count,
            max_reception_size: match (self.max_reception_size, request.max_reception_size) {
                (Some(bpa), Some(cla)) => Some(bpa.min(cla)),
                (bpa, cla) => bpa.or(cla),
            },
        }
    }

    /// Simulate a server crash by forcing unregistration of all
    /// registered components.
    pub async fn crash(&self) {
        let agent = self.last_routing_agent.lock().take();
        if let Some(agent) = agent {
            agent.on_unregister().await;
        }
        let cla = self.last_cla.lock().take();
        if let Some(cla) = cla {
            cla.shutdown().await.expect("the CLA shuts down cleanly");
        }
    }
}

#[async_trait]
impl cla::Registrar for MockBpa {
    async fn register_cla_boxed(
        &self,
        _name: &str,
        factory: cla::BoxFactory<'_>,
    ) -> cla::Result<()> {
        let request = factory.request();
        let terms = self.negotiate(&request);
        *self.declared_request.lock() = Some(request);
        *self.settled_terms.lock() = Some(terms);

        let sink = Arc::new(MockClaSink::new());
        *self.last_cla_sink.lock() = Some(sink.clone());

        let cla = factory.build(cla::Context::new(sink, self.node_ids.clone(), terms))?;
        cla.start().await?;
        *self.last_cla.lock() = Some(cla);
        Ok(())
    }
}

#[async_trait]
impl BpaRegistration for MockBpa {
    async fn register_raw_service(
        &self,
        _service_id: ServiceId,
        service: Arc<dyn raw::Service>,
    ) -> service::Result<Eid> {
        let endpoint: Eid = "ipn:1.42".parse().unwrap();
        let sink = Arc::new(MockRawServiceSink::new());
        *self.last_raw_service.lock() = Some(service.clone());
        service
            .on_register(&endpoint, Box::new(RawServiceSinkWrapper(sink)))
            .await;
        Ok(endpoint)
    }

    async fn register_service(
        &self,
        _service_id: ServiceId,
        service: Arc<dyn Service>,
    ) -> service::Result<Eid> {
        let endpoint: Eid = "ipn:1.42".parse().unwrap();
        let sink = Arc::new(MockServiceSink::new());
        *self.last_service.lock() = Some(service.clone());
        service
            .on_register(&endpoint, Box::new(ServiceSinkWrapper(sink)))
            .await;
        Ok(endpoint)
    }

    async fn register_dynamic_raw_service(
        &self,
        service: Arc<dyn raw::Service>,
    ) -> service::Result<Eid> {
        self.register_raw_service(ServiceId::Ipn(0), service).await
    }

    async fn register_dynamic_service(&self, service: Arc<dyn Service>) -> service::Result<Eid> {
        self.register_service(ServiceId::Ipn(0), service).await
    }

    async fn register_routing_agent(
        &self,
        _name: String,
        agent: Arc<dyn routing::Agent>,
    ) -> routing::Result<Vec<NodeId>> {
        let node_ids = Vec::<NodeId>::from(self.node_ids.as_ref());
        let sink = Arc::new(MockRoutingSink::new());
        *self.last_routing_sink.lock() = Some(sink.clone());
        *self.last_routing_agent.lock() = Some(agent.clone());

        agent
            .on_register(Box::new(RoutingSinkWrapper(sink)), &node_ids)
            .await;

        Ok(node_ids)
    }
}

// ── Sink wrappers (delegate to Arc<Mock>) ─────────────────────────────

struct RoutingSinkWrapper(Arc<MockRoutingSink>);
struct RawServiceSinkWrapper(Arc<MockRawServiceSink>);
struct ServiceSinkWrapper(Arc<MockServiceSink>);

#[async_trait]
impl routing::Sink for RoutingSinkWrapper {
    async fn unregister(&self) {
        self.0.unregister().await;
    }
    async fn add_route(
        &self,
        p: hardy_eid_patterns::EidPattern,
        a: routing::RouteAction,
        pri: u32,
    ) -> routing::Result<bool> {
        self.0.add_route(p, a, pri).await
    }
    async fn remove_route(
        &self,
        p: &hardy_eid_patterns::EidPattern,
        a: &routing::RouteAction,
        pri: u32,
    ) -> routing::Result<bool> {
        self.0.remove_route(p, a, pri).await
    }
}

#[async_trait]
impl raw::Sink for RawServiceSinkWrapper {
    async fn unregister(&self) {
        self.0.unregister().await;
    }
    async fn send(
        &self,
        s: &mut dyn hardy_bpa::stream::Receiver<hardy_bpa::stream::Segment>,
    ) -> service::Result<hardy_bpv7::bundle::Id> {
        self.0.send(s).await
    }
}

#[async_trait]
impl Sink for ServiceSinkWrapper {
    async fn unregister(&self) {
        self.0.unregister().await;
    }
    async fn send(
        &self,
        dest: Eid,
        data: hardy_bpa::Bytes,
        lt: core::time::Duration,
        opts: Option<SendOptions>,
    ) -> service::Result<hardy_bpv7::bundle::Id> {
        self.0.send(dest, data, lt, opts).await
    }
}

// ── Server helpers ────────────────────────────────────────────────────

/// The loopback host for test servers: IPv6 when available, IPv4 as a
/// fallback (some sandboxes have no `::1`).
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

/// Start a gRPC server with the specified services.
/// Returns the gRPC address string and the task pool (cancel to stop).
pub async fn start_server(
    bpa: &Arc<MockBpa>,
    service_names: &[&str],
) -> (String, hardy_async::TaskPool) {
    // Port 0: the kernel assigns a free port, so concurrent test binaries
    // never collide. `GrpcServer::new` binds the socket, so connections are
    // accepted (into the backlog) from here on and no listen-wait is needed.
    let host = loopback_host();
    let addr: std::net::SocketAddr = format!("{host}:0").parse().unwrap();

    let tasks = hardy_async::TaskPool::new();
    let config = hardy_proto::server::Config {
        address: addr,
        services: service_names.iter().map(|s| s.to_string()).collect(),
    };

    let server = hardy_proto::server::GrpcServer::new(&config, bpa.clone())
        .expect("Failed to create gRPC server");
    let grpc_addr = format!("http://{}", server.local_addr());
    let cancel = tasks.cancel_token().clone();
    hardy_async::spawn!(tasks, "grpc_server", async move {
        if let Err(e) = server.serve(cancel).await {
            tracing::error!("gRPC server failed: {e}");
        }
    });

    (grpc_addr, tasks)
}
