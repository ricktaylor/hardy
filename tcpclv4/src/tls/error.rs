//! The TLS error taxonomy: every failure that concerns a file or
//! directory names it, so certificate problems stay attributable to the
//! exact path that caused them.

use std::path::PathBuf;

use rustls::pki_types::pem;
use thiserror::Error;

/// Shorthand for results whose error is [`enum@Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// Errors from loading or validating TLS material, returned by
/// [`TlsBuilder::build`](super::TlsBuilder::build).
#[derive(Error, Debug)]
pub enum Error {
    /// Reading or parsing the certificate PEM file failed.
    #[error("Failed to load certificate from '{}': {source}", .path.display())]
    LoadCertificate {
        path: PathBuf,
        #[source]
        source: pem::Error,
    },

    /// Reading or parsing the private key PEM file failed.
    #[error("Failed to load private key from '{}': {source}", .path.display())]
    LoadPrivateKey {
        path: PathBuf,
        #[source]
        source: pem::Error,
    },

    /// rustls rejected the certificate/private-key pair for the server role.
    #[error("Failed to build the server configuration for '{}': {source}", .path.display())]
    BuildServerConfig {
        path: PathBuf,
        #[source]
        source: rustls::Error,
    },

    /// rustls rejected the certificate/private-key pair for the client role.
    #[error("Failed to build the client configuration for '{}': {source}", .path.display())]
    BuildClientConfig {
        path: PathBuf,
        #[source]
        source: rustls::Error,
    },

    /// Building the client-certificate verifier from the trust anchors failed.
    #[error("Failed to build the client-certificate verifier from '{}': {source}", .path.display())]
    BuildClientVerifier {
        path: PathBuf,
        #[source]
        source: rustls::server::VerifierBuilderError,
    },

    /// No trust anchor was chained before building.
    #[error(
        "No TLS trust anchor is configured: set ca-certs, \
        or insecure-skip-verify for testing only"
    )]
    NoTrustAnchor,

    /// Client verification is enabled without an identity to serve with.
    #[error("client-auth requires an identity (certificate and key)")]
    ClientAuthWithoutIdentity,

    /// Client verification is enabled with insecure trust, which has no
    /// anchors to verify dialers against.
    #[error("client-auth requires ca-certs trust anchors: insecure-skip-verify provides none")]
    ClientAuthWithoutAnchors,

    /// Enumerating the CA certificate directory or one of its entries failed.
    #[error("Failed to read CA certificate directory '{}': {source}", .path.display())]
    ReadCaCerts {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The trust store rejected a parsed CA certificate.
    #[error("Failed to add CA certificate from '{}' to the trust store: {source}", .path.display())]
    AddTrustAnchor {
        path: PathBuf,
        #[source]
        source: rustls::Error,
    },

    /// The configured CA certificate directory does not exist.
    #[error("CA certificate directory '{}' does not exist", .path.display())]
    CaCertsMissing { path: PathBuf },

    /// The configured CA certificate path is not a directory.
    #[error("CA certificate path '{}' must be a directory, not a file", .path.display())]
    CaCertsNotADirectory { path: PathBuf },

    /// The CA certificate directory contained no loadable certificates.
    #[error("No certificates found in CA certificate directory '{}'", .path.display())]
    CaCertsEmpty { path: PathBuf },
}
