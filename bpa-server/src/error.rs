#[cfg(feature = "grpc")]
use std::net::SocketAddr;

// Errors returned by the BPA server during startup.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Failed to read configuration: {0}")]
    ConfigRead(#[from] config::ConfigError),

    #[cfg(feature = "grpc")]
    #[error("failed to bind gRPC listener on {address}")]
    Bind {
        address: SocketAddr,
        #[source]
        source: std::io::Error,
    },
}
