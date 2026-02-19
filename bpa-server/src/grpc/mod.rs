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

pub fn init(config: &Config, bpa: &Arc<hardy_bpa::bpa::Bpa>, tasks: &hardy_async::TaskPool) {
    // Convert to proto server config
    let proto_config = hardy_proto::server::Config {
        address: config.address,
        services: config.services.clone(),
    };

    // Bpa implements BpaRegistration, so we can pass it as dyn BpaRegistration
    let bpa: Arc<dyn hardy_bpa::bpa::BpaRegistration> = bpa.clone();

    hardy_proto::server::init(&proto_config, &bpa, tasks);
}
