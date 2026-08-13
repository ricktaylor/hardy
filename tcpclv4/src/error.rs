use std::net::SocketAddr;

use crate::{session, tls};

/// Convenience alias used throughout the crate so callers can write
/// `error::Result<T>` instead of `Result<T, error::Error>`.
pub type Result<T> = std::result::Result<T, Error>;

/// The generic error for everything related to TCPCLv4: assembling and
/// registering the entity, with each sub-concept's own error wrapped as a
/// variant ([`tls::Error`], `session::Error`).
#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// Binding a passive listening element's socket failed at
    /// `Tcpclv4Builder::build`, so port conflicts and missing privileges
    /// surface as construction errors instead of background-task logs.
    #[error("Failed to bind the listener on '{address}': {source}")]
    BindListener {
        address: SocketAddr,
        #[source]
        source: std::io::Error,
    },

    /// Listeners are configured with TLS required but no identity to
    /// serve: they could accept neither TLS nor plaintext sessions.
    #[error(
        "TLS is required but no identity (certificate and key) is configured: \
        the listeners could serve neither TLS nor plaintext"
    )]
    RequiredTlsWithoutIdentity,

    /// The entity is already registered with a BPA; the sink and the
    /// bound listener sockets were consumed by the first registration.
    #[error("The entity is already registered")]
    AlreadyRegistered,

    /// A TLS error surfaced at the crate boundary: loading or validating
    /// the TLS material built by `tls::Tls::builder()`.
    #[error("TLS error: {0}")]
    Tls(#[from] tls::Error),

    /// A session error surfaced at the crate boundary: how a session, or
    /// the establishment of one (for example a dial from
    /// `Tcpclv4::connect`), ended.
    #[error("Session error: {0}")]
    Session(#[from] session::Error),
}
