// TLS material for the CLA: the loaded rustls configurations for both
// roles. This module owns certificate material and nothing else; it knows
// nothing about sockets, sessions, or the handshake. Construction from the
// user-facing `config::TlsConfig` lives in `builder`; the deliberately
// insecure debug trust policy lives in `verifier`.

use std::path::PathBuf;
use std::sync::Arc;

use rustls::{ClientConfig, ServerConfig, pki_types::pem};
use thiserror::Error;
use tokio_rustls::{TlsAcceptor, TlsConnector};

mod builder;
mod verifier;

pub use self::builder::TlsBuilder;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Error, Debug)]
pub enum Error {
    // Reading or parsing the certificate PEM file failed.
    #[error("Failed to load certificate from '{}': {source}", .path.display())]
    LoadCertificate {
        path: PathBuf,
        #[source]
        source: pem::Error,
    },

    // Reading or parsing the private key PEM file failed.
    #[error("Failed to load private key from '{}': {source}", .path.display())]
    LoadPrivateKey {
        path: PathBuf,
        #[source]
        source: pem::Error,
    },

    // rustls rejected the certificate/private-key pair.
    #[error("Failed to build the server configuration for '{}': {source}", .path.display())]
    BuildServerConfig {
        path: PathBuf,
        #[source]
        source: rustls::Error,
    },

    // Enumerating the CA bundle directory or one of its entries failed.
    #[error("Failed to read CA bundle directory '{}': {source}", .path.display())]
    ReadCaBundle {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    // The trust store rejected a parsed CA certificate.
    #[error("Failed to add CA certificate from '{}' to the trust store: {source}", .path.display())]
    AddTrustAnchor {
        path: PathBuf,
        #[source]
        source: rustls::Error,
    },

    // The configured CA bundle directory does not exist.
    #[error("CA bundle directory '{}' does not exist", .path.display())]
    CaBundleMissing { path: PathBuf },

    // The configured CA bundle path is not a directory.
    #[error("CA bundle path '{}' must be a directory, not a file", .path.display())]
    CaBundleNotADirectory { path: PathBuf },

    // The CA bundle directory contained no loadable certificates.
    #[error("No certificates found in CA bundle directory '{}'", .path.display())]
    CaBundleEmpty { path: PathBuf },
}

// The client role's trust anchor: exactly one source of truth, so "no
// anchor" and "two competing anchors" are unrepresentable. rustls installs
// a single verifier per config; a CA bundle alongside the self-signed
// verifier would be silently ignored.
#[derive(Debug)]
pub enum Trust {
    // Verify peers against the CA certificates found in this directory of
    // PEM files.
    CaBundle(PathBuf),
    // Accept any peer certificate chain, self-signed included, with no
    // trust validation. Testing only.
    Insecure,
}

// The loaded TLS material, exposed as the two roles a session can play: an
// acceptor for the passive side (present only when a certificate is
// configured) and a connector for the dialing side (always available).
#[derive(Debug)]
pub struct Tls {
    server: Option<Arc<ServerConfig>>,
    client: Arc<ClientConfig>,
    server_name: Option<String>,
}

impl Tls {
    // Start building TLS material. The client role is always built, so its
    // trust anchor is the one mandatory input. The optional server role and
    // the SNI override chain via [`TlsBuilder::server`] and
    // [`TlsBuilder::server_name`].
    pub fn builder(trust: Trust) -> TlsBuilder {
        TlsBuilder::new(trust)
    }

    // Acceptor for the passive (listener) role; `None` when no certificate
    // is configured, which also gates whether the listener offers TLS.
    pub fn acceptor(&self) -> Option<TlsAcceptor> {
        self.server.clone().map(TlsAcceptor::from)
    }

    // Connector for the active (dialing) role.
    pub fn connector(&self) -> TlsConnector {
        TlsConnector::from(self.client.clone())
    }

    // The configured SNI override presented when dialing.
    pub fn server_name(&self) -> Option<&str> {
        self.server_name.as_deref()
    }
}
