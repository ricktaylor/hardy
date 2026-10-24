//! gRPC server implementations for BPA services.
//!
//! This module provides gRPC server implementations that allow remote CLAs,
//! services, and applications to connect to a BPA instance.

use hardy_async::sync::spin::Once;
use hardy_bpa::services;

use super::*;
use proxy::*;

mod application;
mod cla;
mod routing;
mod service;

// The status a failed service or application registration reaches the
// client as, so a client can act on the code: an id already taken is
// AlreadyExists, an id or scheme this node cannot register is
// InvalidArgument, and a BPA shutting down is Unavailable (retryable).
fn registration_error_to_status(e: services::Error) -> tonic::Status {
    match e {
        services::Error::ServiceIdInUse(_) => tonic::Status::already_exists(e.to_string()),
        services::Error::AdministrativeEndpoint(_)
        | services::Error::DtnInvalidServiceName(_)
        | services::Error::NoIpnNodeId
        | services::Error::NoDtnNodeId
        | services::Error::NodeId(_) => tonic::Status::invalid_argument(e.to_string()),
        services::Error::Disconnected => tonic::Status::unavailable(e.to_string()),
        other => tonic::Status::internal(other.to_string()),
    }
}

fn to_timestamp(t: time::OffsetDateTime) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: (t.unix_timestamp_nanos() / 1_000_000_000) as i64,
        nanos: (t.unix_timestamp_nanos() % 1_000_000_000) as i32,
    }
}

/// Configuration for the gRPC server.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default, deny_unknown_fields))]
pub struct Config {
    /// Address to bind the gRPC server to.
    pub address: std::net::SocketAddr,
    /// List of services to enable: "cla", "service", "application", "routing"
    pub services: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            address: std::net::SocketAddr::new(
                std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
                50051,
            ),
            services: Vec::new(),
        }
    }
}

/// A gRPC server that exposes BPA registration services to remote clients.
///
/// The listening socket is bound by [`new()`](GrpcServer::new), so a bind
/// failure surfaces at construction and a config address with port 0 gets a
/// kernel-assigned port, readable via [`local_addr()`](GrpcServer::local_addr).
/// The server does not spawn any tasks itself — call [`serve()`](GrpcServer::serve)
/// to get a future, and spawn it in your own runtime.
pub struct GrpcServer {
    routes: tonic::service::Routes,
    listener: std::net::TcpListener,
    local_addr: std::net::SocketAddr,
    session_tasks: hardy_async::TaskPool,
}

impl GrpcServer {
    /// Build a gRPC server with the configured services, bound to
    /// `config.address`.
    pub fn new(
        config: &Config,
        bpa: Arc<dyn hardy_bpa::bpa::BpaRegistration>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        if config.services.is_empty() {
            return Err("No gRPC services configured".into());
        }

        let tasks = hardy_async::TaskPool::new();
        let mut routes = tonic::service::Routes::builder();
        for svc in &config.services {
            match svc.as_str() {
                "application" => {
                    routes.add_service(application::new_application_service(&bpa, &tasks));
                }
                "cla" => {
                    routes.add_service(cla::new_cla_service(&bpa, &tasks));
                }
                "service" => {
                    routes.add_service(service::new_endpoint_service(&bpa, &tasks));
                }
                "routing" => {
                    routes.add_service(routing::new_routing_agent_service(&bpa, &tasks));
                }
                s => {
                    warn!("Ignoring unknown gRPC service {s}");
                }
            }
        }

        let listener = std::net::TcpListener::bind(config.address)?;
        listener.set_nonblocking(true)?;
        let local_addr = listener.local_addr()?;

        info!(
            "gRPC server hosting {:?}, listening on {local_addr}",
            config.services
        );

        Ok(Self {
            routes: routes.routes(),
            listener,
            local_addr,
            session_tasks: tasks,
        })
    }

    /// The address the listening socket is bound to. With a port-0 config
    /// address this carries the kernel-assigned port. The socket accepts
    /// connections into its backlog from construction, before
    /// [`serve()`](GrpcServer::serve) runs.
    pub fn local_addr(&self) -> std::net::SocketAddr {
        self.local_addr
    }

    /// Serve until cancelled, then shut down session tasks.
    pub async fn serve(
        self,
        cancel: hardy_async::CancellationToken,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (health_reporter, health_service) = tonic_health::server::health_reporter();
        health_reporter
            .set_service_status("", tonic_health::ServingStatus::Serving)
            .await;
        // NODELAY matches what `serve_with_shutdown` would apply through the
        // builder's default `tcp_nodelay(true)` when it binds the socket
        // itself.
        let incoming = tonic::transport::server::TcpIncoming::from(
            tokio::net::TcpListener::from_std(self.listener)?,
        )
        .with_nodelay(Some(true));
        tonic::transport::Server::builder()
            .add_routes(self.routes)
            .add_service(health_service)
            .serve_with_incoming_shutdown(incoming, cancel.cancelled())
            .await?;
        self.session_tasks.shutdown().await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::net::{SocketAddr, TcpListener};

    use hardy_bpa::{
        bpa::{Bpa, BpaRegistration},
        cla::Lanes,
        services::{Service, ServiceSink, StatusNotify},
        stream::{Receiver, Segment},
    };
    use hardy_bpv7::{
        bundle::Id,
        eid::{Eid, IpnNodeId, NodeId, Service as ServiceId},
        status_report::ReasonCode,
    };
    use time::OffsetDateTime;
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::ReceiverStream;

    use super::*;
    use crate::proto::service::{
        AppToBpa, RegisterRequest, ServiceToBpa, app_to_bpa, application_client::ApplicationClient,
        register_request, service_client::ServiceClient, service_to_bpa,
    };

    // The loopback host for test servers: IPv6 when available, IPv4 as a
    // fallback (some sandboxes have no ::1).
    fn loopback() -> SocketAddr {
        if TcpListener::bind(("::1", 0)).is_ok() {
            "[::1]:0".parse().unwrap()
        } else {
            "127.0.0.1:0".parse().unwrap()
        }
    }

    // A real BPA (node 1) behind a gRPC server hosting the service and
    // application endpoints, on an ephemeral port.
    async fn serve() -> (Arc<Bpa>, String, hardy_async::TaskPool) {
        let node_ids = [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice()
        .try_into()
        .unwrap();
        let bpa = Arc::new(Bpa::builder().node_ids(node_ids).build().await.unwrap());
        bpa.start(false).await;

        let server = GrpcServer::new(
            &Config {
                address: loopback(),
                services: vec!["service".into(), "application".into()],
            },
            bpa.clone(),
        )
        .unwrap();
        let addr = format!("http://{}", server.local_addr());
        let tasks = hardy_async::TaskPool::new();
        let cancel = tasks.cancel_token().clone();
        hardy_async::spawn!(tasks, "test_grpc_server", async move {
            server.serve(cancel).await.unwrap();
        });
        (bpa, addr, tasks)
    }

    // A service that takes nothing: it only occupies a service id, keeping
    // its sink (dropping it would unregister the service).
    #[derive(Default)]
    struct NullService {
        sink: Once<Box<dyn ServiceSink>>,
    }

    #[async_trait]
    impl Service for NullService {
        async fn on_register(&self, _endpoint: &Eid, sink: Box<dyn ServiceSink>) {
            self.sink.call_once(|| sink);
        }

        async fn on_unregister(&self) {}

        async fn on_deliver(
            &self,
            _bundle_id: &Id,
            _expiry: OffsetDateTime,
            _total_len: u64,
            _stream: &mut dyn Receiver<Segment>,
        ) -> services::Result<()> {
            Ok(())
        }

        async fn on_status_notify(
            &self,
            _bundle_id: &Id,
            _from: &Eid,
            _kind: StatusNotify,
            _reason: ReasonCode,
            _timestamp: Option<OffsetDateTime>,
        ) {
        }
    }

    fn register_request(lanes: Option<register_request::Lanes>) -> RegisterRequest {
        RegisterRequest {
            service_id: Some(register_request::ServiceId::Ipn(42)),
            lanes,
        }
    }

    // Send a raw registration on the service endpoint and return the code
    // the stream ends with, or None if it answered with a registration.
    async fn register_raw_service(addr: &str, request: RegisterRequest) -> Option<tonic::Code> {
        let (tx, rx) = mpsc::channel(1);
        tx.send(ServiceToBpa {
            msg_id: 0,
            msg: Some(service_to_bpa::Msg::Register(request)),
        })
        .await
        .unwrap();
        let mut responses = ServiceClient::connect(addr.to_string())
            .await
            .unwrap()
            .register(ReceiverStream::new(rx))
            .await
            .unwrap()
            .into_inner();
        responses.message().await.err().map(|status| status.code())
    }

    // As `register_raw_service`, on the application endpoint.
    async fn register_raw_application(addr: &str, request: RegisterRequest) -> Option<tonic::Code> {
        let (tx, rx) = mpsc::channel(1);
        tx.send(AppToBpa {
            msg_id: 0,
            msg: Some(app_to_bpa::Msg::Register(request)),
        })
        .await
        .unwrap();
        let mut responses = ApplicationClient::connect(addr.to_string())
            .await
            .unwrap()
            .register(ReceiverStream::new(rx))
            .await
            .unwrap()
            .into_inner();
        responses.message().await.err().map(|status| status.code())
    }

    // Registration errors reach the client with codes it can act on.
    #[test]
    fn registration_errors_map_to_actionable_codes() {
        let code = |e| registration_error_to_status(e).code();
        assert_eq!(
            code(services::Error::ServiceIdInUse("42".into())),
            tonic::Code::AlreadyExists
        );
        assert_eq!(
            code(services::Error::AdministrativeEndpoint("0".into())),
            tonic::Code::InvalidArgument
        );
        assert_eq!(
            code(services::Error::NodeId(
                hardy_bpa::node_ids::Error::NoDtnNodeId
            )),
            tonic::Code::InvalidArgument
        );
        assert_eq!(
            code(services::Error::Disconnected),
            tonic::Code::Unavailable
        );
    }

    // A registration declaring a zero lane limit is rejected before it
    // reaches the BPA, and the rejection reaches the client as the stream's
    // InvalidArgument status, on both endpoints. The id stays free.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_zero_lane_limit_rejects_the_registration() {
        let (bpa, addr, tasks) = serve().await;
        let zero = || register_request(Some(register_request::Lanes::Limited(0)));

        assert_eq!(
            register_raw_service(&addr, zero()).await,
            Some(tonic::Code::InvalidArgument)
        );
        assert_eq!(
            register_raw_application(&addr, zero()).await,
            Some(tonic::Code::InvalidArgument)
        );

        // Nothing registered: the id is still free.
        bpa.register_service(
            ServiceId::Ipn(42),
            Arc::new(NullService::default()),
            Lanes::Serial,
        )
        .await
        .expect("the rejected registrations left the id free");

        tasks.shutdown().await;
        bpa.shutdown().await;
    }

    // A registration for an id already taken reaches the client as
    // AlreadyExists, on both endpoints.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_taken_service_id_is_already_exists() {
        let (bpa, addr, tasks) = serve().await;
        bpa.register_service(
            ServiceId::Ipn(42),
            Arc::new(NullService::default()),
            Lanes::Serial,
        )
        .await
        .unwrap();

        assert_eq!(
            register_raw_service(&addr, register_request(None)).await,
            Some(tonic::Code::AlreadyExists)
        );
        assert_eq!(
            register_raw_application(&addr, register_request(None)).await,
            Some(tonic::Code::AlreadyExists)
        );

        tasks.shutdown().await;
        bpa.shutdown().await;
    }
}
