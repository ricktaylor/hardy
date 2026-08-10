//! TLS material for the CLA: the loaded rustls configurations for both
//! roles. This module owns certificate material and nothing else; it knows
//! nothing about sockets, sessions, or the handshake. Construction chains
//! from [`Tls::builder`], and the built [`Tls`] is handed to
//! [`Tcpclv4Builder::tls`](crate::builder::Tcpclv4Builder::tls).
//! The deliberately insecure debug trust policy lives in `verifier`.

use std::sync::Arc;

use rustls::{ClientConfig, ServerConfig};
use tokio_rustls::{TlsAcceptor, TlsConnector};

mod builder;
mod error;
mod verifier;

pub use self::builder::{ClientAuth, DangerousTlsBuilder, TlsBuilder};
pub use self::error::{Error, Result};

/// The loaded TLS material, exposed as the two roles a session can play: an
/// acceptor for the passive side (present only when an identity is
/// configured) and a connector for the dialing side (always available).
#[derive(Debug)]
pub struct Tls {
    required: bool,
    server: Option<Arc<ServerConfig>>,
    client: Arc<ClientConfig>,
    server_name: Option<String>,
}

impl Tls {
    /// Start building TLS material. The dialing role is always built, so a
    /// trust anchor is the one mandatory input: chain
    /// [`TlsBuilder::ca_certs`] for the secure path, or
    /// [`TlsBuilder::dangerous`] for the loudly marked insecure one. The
    /// node identity, client verification, and the SNI override chain via
    /// [`TlsBuilder::identity`], [`TlsBuilder::client_auth`], and
    /// [`TlsBuilder::server_name`].
    pub fn builder() -> TlsBuilder {
        TlsBuilder::new()
    }

    /// Whether sessions that do not negotiate TLS must be refused, as
    /// chained with [`TlsBuilder::required`].
    pub fn is_required(&self) -> bool {
        self.required
    }

    // Whether an identity is configured: only then can this material
    // serve the TLS server role on the accepting side.
    pub(crate) fn has_identity(&self) -> bool {
        self.server.is_some()
    }

    // Whether this material demands TLS while lacking an identity to
    // serve it: a listener could then accept neither TLS (no server
    // role) nor plaintext (refused by policy).
    pub(crate) fn required_without_identity(&self) -> bool {
        self.required && self.server.is_none()
    }

    // Acceptor for the passive (listener) role; `None` when no identity
    // is configured, which also gates whether the listener offers TLS.
    pub(crate) fn acceptor(&self) -> Option<TlsAcceptor> {
        self.server.clone().map(TlsAcceptor::from)
    }

    pub(crate) fn connector(&self) -> TlsConnector {
        TlsConnector::from(self.client.clone())
    }

    pub(crate) fn server_name(&self) -> Option<&str> {
        self.server_name.as_deref()
    }
}
