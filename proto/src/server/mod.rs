//! gRPC server implementations for BPA services.
//!
//! This module provides gRPC server implementations that allow remote CLAs,
//! services, and applications to connect to a BPA instance.

use super::*;
use hardy_async::sync::spin::Once;
use proxy::*;

mod application;
mod cla;
mod service;

fn to_timestamp(t: time::OffsetDateTime) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: (t.unix_timestamp_nanos() / 1_000_000_000) as i64,
        nanos: (t.unix_timestamp_nanos() % 1_000_000_000) as i32,
    }
}

pub use application::new_application_service;
pub use cla::new_cla_service;
pub use service::new_endpoint_service;

/// Configuration for the gRPC server.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct Config {
    /// Address to bind the gRPC server to.
    pub address: std::net::SocketAddr,
    /// List of services to enable: "cla", "service", "application"
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

/// Initialize and start the gRPC server.
///
/// # Arguments
///
/// * `config` - Server configuration
/// * `bpa` - BPA registration interface (can be local Bpa or remote)
/// * `cancel_token` - Token to signal server shutdown
/// * `task_tracker` - Task tracker for the server task
pub fn init(
    config: &Config,
    bpa: &Arc<dyn hardy_bpa::bpa::BpaRegistration>,
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
                routes.add_service(application::new_application_service(bpa));
            }
            "cla" => {
                routes.add_service(cla::new_cla_service(bpa));
            }
            "service" => {
                routes.add_service(service::new_endpoint_service(bpa));
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
            .expect("Failed to start gRPC server")
    });

    info!(
        "gRPC server hosting {:?}, listening on {}",
        config.services, config.address
    )
}
