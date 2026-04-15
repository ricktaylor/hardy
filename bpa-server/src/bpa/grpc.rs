use super::*;
use serde::{Deserialize, Serialize};

// Configuration for the gRPC registration server.
#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    // Socket address to listen on (default: `[::1]:50051`).
    pub address: std::net::SocketAddr,
    // Additional gRPC service names to register (default: empty).
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

// Start the gRPC server and register it with the BPA task pool.
#[cfg(feature = "grpc")]
pub fn init(
    config: &Config,
    bpa: &Arc<dyn hardy_bpa::bpa::BpaRegistration>,
    tasks: &hardy_async::TaskPool,
) {
    let proto_config = hardy_proto::server::Config {
        address: config.address,
        services: config.services.clone(),
    };

    hardy_proto::server::init(&proto_config, bpa, tasks);
}

// No-op stub when the `grpc` feature is disabled; logs a warning.
#[cfg(not(feature = "grpc"))]
pub fn init(
    _config: &Config,
    _bpa: &Arc<dyn hardy_bpa::bpa::BpaRegistration>,
    _tasks: &hardy_async::TaskPool,
) {
    warn!("Ignoring gRPC configuration as it is disabled at compile time");
}
