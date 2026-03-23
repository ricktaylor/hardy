use super::*;
use serde::{Deserialize, Serialize};

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

#[cfg(not(feature = "grpc"))]
pub fn init(
    _config: &Config,
    _bpa: &Arc<dyn hardy_bpa::bpa::BpaRegistration>,
    _tasks: &hardy_async::TaskPool,
) {
    warn!("Ignoring gRPC configuration as it is disabled at compile time");
}
