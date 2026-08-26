#[cfg(feature = "grpc")]
use std::{net::SocketAddr, path::PathBuf};

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

    #[cfg(feature = "grpc")]
    #[error("failed to read gRPC TLS file '{}'", path.display())]
    TlsRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[cfg(feature = "grpc")]
    #[error("gRPC client-auth requires ca-certs")]
    TlsClientAuthWithoutCaCerts,

    #[cfg(feature = "grpc")]
    #[error("gRPC TLS configuration is invalid: {0}")]
    Tls(#[from] tonic::transport::Error),
}
