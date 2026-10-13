//! The TLS error taxonomy: every failure that concerns a file or directory names it, so certificate problems stay attributable to the exact path that caused them.

use std::{io, path::PathBuf};

use quinn::crypto::rustls::NoInitialCipherSuite;
use rustls::{pki_types::pem, server::VerifierBuilderError};
use thiserror::Error;

/// A specialized `Result` type for TLS material.
pub type Result<T> = core::result::Result<T, Error>;

/// Errors from loading or validating TLS material, returned by [`TlsBuilder::build`](super::TlsBuilder::build).
#[derive(Debug, Error)]
pub enum Error {
    /// No trust anchor was chained before building.
    #[error("no TLS trust anchor is configured: set ca-certs")]
    NoTrustAnchor,

    /// No identity was chained before building; Qubicle requires both peers to present a certificate (Section 4.2).
    #[error("no TLS identity (certificate and key) is configured")]
    NoIdentity,

    /// Reading or parsing the certificate PEM file failed.
    #[error("failed to load the certificate from '{}': {source}", .path.display())]
    LoadCertificate {
        /// The certificate file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: pem::Error,
    },

    /// Reading or parsing the private key PEM file failed.
    #[error("failed to load the private key from '{}': {source}", .path.display())]
    LoadPrivateKey {
        /// The private key file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: pem::Error,
    },

    /// The crypto provider rejected the private key, or found that it does not match the certificate.
    #[error("the private key from '{}' is unusable with its certificate: {source}", .path.display())]
    LoadIdentity {
        /// The private key file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: rustls::Error,
    },

    /// The configured CA certificate directory does not exist.
    #[error("CA certificate directory '{}' does not exist", .path.display())]
    CaCertsMissing {
        /// The configured directory.
        path: PathBuf,
    },

    /// The configured CA certificate path is not a directory.
    #[error("CA certificate path '{}' must be a directory, not a file", .path.display())]
    CaCertsNotADirectory {
        /// The configured path.
        path: PathBuf,
    },

    /// Enumerating the CA certificate directory or one of its entries failed.
    #[error("failed to read CA certificate directory '{}': {source}", .path.display())]
    ReadCaCerts {
        /// The configured directory.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },

    /// The CA certificate directory contained no loadable certificates.
    #[error("no certificates found in CA certificate directory '{}'", .path.display())]
    CaCertsEmpty {
        /// The configured directory.
        path: PathBuf,
    },

    /// The trust store rejected a parsed CA certificate.
    #[error("failed to add the CA certificate from '{}' to the trust store: {source}", .path.display())]
    AddTrustAnchor {
        /// The file the certificate came from.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: rustls::Error,
    },

    /// Building the client-certificate verifier from the trust anchors failed.
    #[error("failed to build the client-certificate verifier: {0}")]
    BuildClientVerifier(#[source] VerifierBuilderError),

    /// rustls rejected the configuration, for example a crypto provider without TLS 1.3.
    #[error("failed to build the TLS configuration: {0}")]
    Config(#[source] rustls::Error),

    /// The crypto provider offers no cipher suite usable for QUIC Initial packets.
    #[error("failed to build the QUIC TLS configuration: {0}")]
    Quic(#[from] NoInitialCipherSuite),
}
