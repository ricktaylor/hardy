// Construction of [`Tls`]: certificate and key loading via rustls's own
// PEM API, CA-directory scanning, and the handshake configurations built
// once from them.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use quinn::crypto::rustls::QuicServerConfig;
use rustls::{
    RootCertStore, ServerConfig,
    crypto::aws_lc_rs,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
    server::{NoServerSessionStorage, WebPkiClientVerifier},
    sign::{CertifiedKey, SingleCertAndKey},
    version::TLS13,
};
use tracing::{debug, info};

use super::{Error, Result, Tls, verifier::ClientVerifier};
use crate::ALPN;

/// Builder for [`Tls`]. Obtain one from [`Tls::builder`], chain both inputs, then load them with [`build()`](Self::build).
///
/// Both inputs are mandatory: Qubicle requires each peer to present a certificate and to validate the other's (Section 4.2), so a node needs both an identity and trust anchors.
#[must_use = "a TlsBuilder does nothing unless `build()` is called"]
#[derive(Debug, Default)]
pub struct TlsBuilder {
    ca_certs: Option<PathBuf>,
    identity: Option<(PathBuf, PathBuf)>,
}

impl TlsBuilder {
    /// Verify peers against the CA certificates found in this directory of PEM files.
    pub fn ca_certs(mut self, dir: PathBuf) -> Self {
        self.ca_certs = Some(dir);
        self
    }

    /// The node's identity: the PEM certificate chain presented to peers, end-entity certificate first, and its private key (PKCS#8, PKCS#1, or SEC1). One pair serves both handshake roles. The certificate should carry the node's Node ID in a BundleEID otherName (RFC 9174 Section 4.4.2), since that is how peers identify the node.
    pub fn identity(mut self, cert_file: PathBuf, key_file: PathBuf) -> Self {
        self.identity = Some((cert_file, key_file));
        self
    }

    /// Loads and validates the TLS material described by the chained inputs.
    ///
    /// # Errors
    ///
    /// [`Error::NoTrustAnchor`] or [`Error::NoIdentity`] when an input was not chained, both judged before any file is read, or another [`enum@Error`] when certificate material fails to load or validate.
    pub fn build(self) -> Result<Tls> {
        let ca_certs = self.ca_certs.ok_or(Error::NoTrustAnchor)?;
        let (cert_path, key_path) = self.identity.ok_or(Error::NoIdentity)?;

        let provider = Arc::new(aws_lc_rs::default_provider());

        let certs = load_ca_certs(&ca_certs)?;
        if certs.is_empty() {
            return Err(Error::CaCertsEmpty { path: ca_certs });
        }
        let mut roots = RootCertStore::empty();
        for (path, cert) in certs {
            roots
                .add(cert)
                .map_err(|source| Error::AddTrustAnchor { path, source })?;
        }
        info!(
            "loaded {} CA certificate(s) from '{}'",
            roots.len(),
            ca_certs.display()
        );
        let roots = Arc::new(roots);

        let chain = CertificateDer::pem_file_iter(&cert_path)
            .and_then(|iter| iter.collect::<core::result::Result<Vec<_>, _>>())
            .map_err(|source| Error::LoadCertificate {
                path: cert_path.clone(),
                source,
            })?;
        let key =
            PrivateKeyDer::from_pem_file(&key_path).map_err(|source| Error::LoadPrivateKey {
                path: key_path.clone(),
                source,
            })?;
        let identity = Arc::new(CertifiedKey::from_der(chain, key, &provider).map_err(
            |source| Error::LoadIdentity {
                path: key_path,
                source,
            },
        )?);

        // TODO(revocation): the verifier is built without CRLs, so a
        // compromised-but-unexpired certificate stays valid until it
        // expires or the CA directory is edited.
        let webpki = WebPkiClientVerifier::builder_with_provider(roots.clone(), provider.clone())
            .build()
            .map_err(Error::BuildClientVerifier)?;

        let mut server = ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&TLS13])
            .map_err(Error::Config)?
            .with_client_cert_verifier(Arc::new(ClientVerifier::new(webpki)))
            .with_cert_resolver(Arc::new(SingleCertAndKey::from(identity.clone())));
        server.alpn_protocols = vec![ALPN.to_vec()];
        // Every handshake is a full one: the Node ID check runs in the
        // certificate verifier, which a resumed handshake skips.
        server.session_storage = Arc::new(NoServerSessionStorage {});
        server.send_tls13_tickets = 0;

        Ok(Tls {
            provider,
            roots,
            identity,
            server: Arc::new(QuicServerConfig::try_from(server)?),
        })
    }
}

// Scan `dir` for PEM certificates, pairing each with the file it came from so
// a later trust-store rejection stays attributable. Files that do not parse
// as certificates (keys, serial files, requests) are skipped.
fn load_ca_certs(dir: &Path) -> Result<Vec<(PathBuf, CertificateDer<'static>)>> {
    if !dir.exists() {
        return Err(Error::CaCertsMissing {
            path: dir.to_path_buf(),
        });
    }
    if !dir.is_dir() {
        return Err(Error::CaCertsNotADirectory {
            path: dir.to_path_buf(),
        });
    }

    let read_error = |source| Error::ReadCaCerts {
        path: dir.to_path_buf(),
        source,
    };
    let mut certs = Vec::new();
    for entry in fs::read_dir(dir).map_err(read_error)? {
        let path = entry.map_err(read_error)?.path();
        if path.is_dir() {
            continue;
        }
        match CertificateDer::pem_file_iter(&path)
            .and_then(|iter| iter.collect::<core::result::Result<Vec<_>, _>>())
        {
            Ok(parsed) => certs.extend(parsed.into_iter().map(|cert| (path.clone(), cert))),
            Err(e) => debug!("skipping '{}': not a certificate file: {e}", path.display()),
        }
    }
    Ok(certs)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Both inputs are judged before any file is read: the paths named here
    // do not exist.
    #[test]
    fn a_trust_anchor_is_mandatory() {
        let result = Tls::builder()
            .identity(
                "/nonexistent/cert.pem".into(),
                "/nonexistent/key.pem".into(),
            )
            .build();
        assert!(matches!(result, Err(Error::NoTrustAnchor)));
    }

    #[test]
    fn an_identity_is_mandatory() {
        let result = Tls::builder().ca_certs("/nonexistent/ca".into()).build();
        assert!(matches!(result, Err(Error::NoIdentity)));
    }

    #[test]
    fn a_missing_ca_directory_is_named() {
        let result = Tls::builder()
            .ca_certs("/nonexistent/ca".into())
            .identity(
                "/nonexistent/cert.pem".into(),
                "/nonexistent/key.pem".into(),
            )
            .build();
        match result {
            Err(Error::CaCertsMissing { path }) => assert_eq!(path, Path::new("/nonexistent/ca")),
            other => panic!("expected CaCertsMissing, got {other:?}"),
        }
    }
}
