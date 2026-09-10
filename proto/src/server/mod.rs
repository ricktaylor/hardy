//! gRPC server implementations for BPA services.
//!
//! This module provides gRPC server implementations that allow remote CLAs,
//! services, and applications to connect to a BPA instance.

use super::*;
use hardy_async::sync::spin::Once;
use proxy::*;

mod application;
mod cla;
mod routing;
mod service;

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
