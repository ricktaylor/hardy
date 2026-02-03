use super::*;
use serde::{Deserialize, Serialize};

mod application;
mod cla;
mod service;

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub address: core::net::SocketAddr,

    #[serde(default)]
    pub services: Vec<String>,
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
