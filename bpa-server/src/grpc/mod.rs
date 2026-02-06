use super::*;
use serde::{Deserialize, Serialize};

mod application;
mod cla;
mod service;

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub address: std::net::SocketAddr,
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

pub fn init(
    config: &Config,
    bpa: &Arc<hardy_bpa::bpa::Bpa>,
    cancel_token: &tokio_util::sync::CancellationToken,
    task_tracker: &tokio_util::task::TaskTracker,
) {
    if config.services.is_empty() {
        return;
    }

    // Add gRPC services to HTTP router
    let mut routes = tonic::service::Routes::builder();
    for service in &config.services {
        match service.as_str() {
            "application" => {
                routes.add_service(application::new_service(bpa));
            }
            "cla" => {
                routes.add_service(cla::new_service(bpa));
            }
            "service" => {
                routes.add_service(service::new_service(bpa));
            }
            s => {
                warn!("Ignoring unknown gRPC service {s}");
            }
        }
    }

    // Start serving
    let addr = config.address;
    let cancel_token = cancel_token.clone();
    task_tracker.spawn(async move {
        tonic::transport::Server::builder()
            .add_routes(routes.routes())
            .serve_with_shutdown(addr, cancel_token.cancelled())
            .await
            .trace_expect("Failed to start gRPC server")
    });

    info!(
        "gRPC server hosting {:?}, listening on {}",
        config.services, config.address
    )
}
