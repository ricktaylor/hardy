#[cfg(feature = "grpc")]
use std::{net::SocketAddr, path::PathBuf};

// Errors returned by the BPA server during startup.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to read configuration")]
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
    #[error("invalid gRPC TLS configuration")]
    Tls(#[from] tonic::transport::Error),

    #[cfg(feature = "grpc")]
    #[error("gRPC server failed while serving")]
    Serve(#[source] tonic::transport::Error),
}
