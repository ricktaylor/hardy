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
#[cfg_attr(feature = "serde", serde(default))]
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
/// The server does not spawn any tasks itself — call [`serve()`](GrpcServer::serve)
/// to get a future, and spawn it in your own runtime.
pub struct GrpcServer {
    routes: tonic::service::Routes,
    address: std::net::SocketAddr,
    session_tasks: hardy_async::TaskPool,
}

impl GrpcServer {
    /// Build a gRPC server with the configured services.
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

        info!(
            "gRPC server hosting {:?}, listening on {}",
            config.services, config.address
        );

        Ok(Self {
            routes: routes.routes(),
            address: config.address,
            session_tasks: tasks,
        })
    }

    /// Serve until cancelled, then shut down session tasks.
    pub async fn serve(
        self,
        cancel: hardy_async::CancellationToken,
    ) -> Result<(), tonic::transport::Error> {
        let (health_reporter, health_service) = tonic_health::server::health_reporter();
        health_reporter
            .set_service_status("", tonic_health::ServingStatus::Serving)
            .await;
        tonic::transport::Server::builder()
            .add_routes(self.routes)
            .add_service(health_service)
            .serve_with_shutdown(self.address, cancel.cancelled())
            .await?;
        self.session_tasks.shutdown().await;
        Ok(())
    }
}
