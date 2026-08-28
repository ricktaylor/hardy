// Construction of [`Tls`]: certificate and key loading via rustls's own
// PEM API and CA-directory scanning.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::{
    ClientConfig, RootCertStore, ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
    server::WebPkiClientVerifier,
};
use tracing::{debug, info, warn};

use super::{Error, Result, Tls, verifier::InsecureVerifier};

/// The accept side's client-certificate policy: whether dialers must
/// present a certificate chaining to the trust anchors (mutual TLS).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ClientAuth {
    /// Never request a client certificate; dialers are unauthenticated.
    #[default]
    Off,
    /// Request a certificate and verify one when presented, but accept
    /// dialers without one: the migration step towards [`Required`](Self::Required).
    Optional,
    /// Refuse dialers without a certificate chaining to the trust anchors
    /// (the RFC 9174 Section 4.4.3 baseline).
    Required,
}

/// Deliberately insecure trust policies, kept behind
/// [`TlsBuilder::dangerous`] so the hazard is spelled out at every call
/// site.
#[must_use = "choose a trust policy to return to the TlsBuilder"]
pub struct DangerousTlsBuilder(TlsBuilder);

impl DangerousTlsBuilder {
    /// Accept any peer certificate chain, self-signed included, with no
    /// trust validation: the connection is encrypted, but the peer is
    /// unauthenticated. Testing only. The name is deliberately the loud,
    /// widely recognised spelling of this hazard, and configuration
    /// surfaces are expected to spell it out the same way. Overrides
    /// [`ca_certs`](TlsBuilder::ca_certs) when both are chained; the
    /// override is warned at build time, and the ignored CA certificates
    /// are never loaded.
    pub fn insecure_skip_verify(mut self) -> TlsBuilder {
        self.0.insecure_skip_verify = true;
        self.0
    }
}

/// Builder for [`Tls`]. Obtain one from [`Tls::builder`]; chain the inputs
/// that apply, then load them with [`build()`](Self::build) and hand the
/// material to [`Tcpclv4Builder::tls`](crate::builder::Tcpclv4Builder::tls).
/// A trust anchor is the one mandatory input: chain [`ca_certs`](Self::ca_certs),
/// or [`dangerous`](Self::dangerous) for the loudly marked insecure path.
#[must_use = "a TlsBuilder does nothing unless `build()` is called"]
pub struct TlsBuilder {
    required: bool,

    // The dialing role's trust anchor. rustls installs a single verifier
    // per config, so `insecure_skip_verify` overrides `ca_certs` rather
    // than combining with it, and having neither is a build error.
    ca_certs: Option<PathBuf>,
    insecure_skip_verify: bool,

    identity: Option<(PathBuf, PathBuf)>,
    client_auth: ClientAuth,
    server_name: Option<String>,
}

impl TlsBuilder {
    pub(super) fn new() -> Self {
        Self {
            required: false,
            ca_certs: None,
            insecure_skip_verify: false,
            identity: None,
            client_auth: ClientAuth::default(),
            server_name: None,
        }
    }

    /// Refuse sessions that do not negotiate TLS (RFC 9174 Section 4.3
    /// "Contact Failure"). Default: `false`, where sessions negotiate TLS
    /// when the peer also advertises it and plaintext peers are still
    /// accepted. Combining listeners with an identity-less required-TLS
    /// policy is rejected at build time.
    pub fn required(mut self, required: bool) -> Self {
        self.required = required;
        self
    }

    /// Verify peers against the CA certificates found in this directory of
    /// PEM files: the secure path.
    pub fn ca_certs(mut self, dir: PathBuf) -> Self {
        self.ca_certs = Some(dir);
        self
    }

    /// Gateway to trust policies that disable verification. Nothing behind
    /// this gateway is fit for production.
    pub fn dangerous(self) -> DangerousTlsBuilder {
        DangerousTlsBuilder(self)
    }

    /// The node's identity: the PEM certificate presented to peers and its
    /// private key (PKCS#8, PKCS#1, or SEC1). One pair serves both roles:
    /// the server certificate when accepting, and the client certificate
    /// when a dialed peer requests one (RFC 9174 Section 4.4.2 profiles a
    /// single end-entity certificate per node). Taking both halves in one
    /// call makes a lone certificate or key unrepresentable.
    pub fn identity(mut self, cert_file: PathBuf, key_file: PathBuf) -> Self {
        self.identity = Some((cert_file, key_file));
        self
    }

    /// The accept side's client-certificate policy. Anything other than
    /// [`ClientAuth::Off`] requires an identity to serve with and CA trust
    /// anchors to verify against.
    pub fn client_auth(mut self, policy: ClientAuth) -> Self {
        self.client_auth = policy;
        self
    }

    /// Override the SNI name presented when dialing (for certificates
    /// issued to domain names).
    pub fn server_name(mut self, name: String) -> Self {
        self.server_name = Some(name);
        self
    }

    /// Loads and validates the TLS material described by the chained
    /// inputs.
    ///
    /// # Errors
    ///
    /// Returns an [`enum@Error`] when no trust anchor was chained, when
    /// client verification lacks an identity or CA trust anchors, or when
    /// certificate material fails to load or validate.
    pub fn build(self) -> Result<Tls> {
        // The trust and cross-input rules are judged before any file is
        // touched; an overridden `ca_certs` is never loaded.
        let ca_certs = if self.insecure_skip_verify {
            if self.ca_certs.is_some() {
                warn!(
                    "insecure-skip-verify overrides ca-certs: the CA certificates are \
                    IGNORED and any peer certificate is accepted"
                );
            }
            None
        } else {
            Some(self.ca_certs.ok_or(Error::NoTrustAnchor)?)
        };

        if self.client_auth != ClientAuth::Off {
            if self.identity.is_none() {
                return Err(Error::ClientAuthWithoutIdentity);
            }
            if ca_certs.is_none() {
                return Err(Error::ClientAuthWithoutAnchors);
            }
        }

        // The trust anchors serve both directions: judging the peer's
        // certificate when dialing and, when client verification is
        // enabled, judging dialers' certificates when accepting.
        let anchors = match ca_certs {
            Some(dir) => {
                let certs = Self::load_ca_certs(&dir)?;
                if certs.is_empty() {
                    return Err(Error::CaCertsEmpty { path: dir });
                }

                let mut store = RootCertStore::empty();
                for (file, cert) in certs {
                    store
                        .add(cert)
                        .map_err(|source| Error::AddTrustAnchor { path: file, source })?;
                }
                info!(
                    "Successfully loaded CA certificate(s) from directory (total in store: {})",
                    store.len()
                );
                Some((dir, Arc::new(store)))
            }
            None => None,
        };

        // The identity is loaded once and presented by both roles.
        let identity = if let Some((cert_path, key_path)) = self.identity {
            let certs = CertificateDer::pem_file_iter(&cert_path)
                .and_then(|iter| iter.collect::<std::result::Result<Vec<_>, _>>())
                .map_err(|source| Error::LoadCertificate {
                    path: cert_path.clone(),
                    source,
                })?;
            let key = PrivateKeyDer::from_pem_file(&key_path).map_err(|source| {
                Error::LoadPrivateKey {
                    path: key_path.clone(),
                    source,
                }
            })?;
            Some((cert_path, certs, key))
        } else {
            None
        };

        let client_verifier = if self.client_auth == ClientAuth::Off {
            None
        } else {
            let Some((dir, store)) = &anchors else {
                // Guarded by the cross-input rules above.
                return Err(Error::ClientAuthWithoutAnchors);
            };
            // TODO(revocation): the verifier is built without CRLs
            // (`with_crls`), so a compromised-but-unexpired client
            // certificate stays valid until it expires or the CA
            // directory is edited.
            let mut verifier = WebPkiClientVerifier::builder(store.clone());
            if self.client_auth == ClientAuth::Optional {
                verifier = verifier.allow_unauthenticated();
            }
            Some(
                verifier
                    .build()
                    .map_err(|source| Error::BuildClientVerifier {
                        path: dir.clone(),
                        source,
                    })?,
            )
        };

        let server = if let Some((cert_path, certs, key)) = &identity {
            let builder = match client_verifier {
                Some(verifier) => ServerConfig::builder().with_client_cert_verifier(verifier),
                None => ServerConfig::builder().with_no_client_auth(),
            };
            Some(Arc::new(
                builder
                    .with_single_cert(certs.clone(), key.clone_key())
                    .map_err(|source| Error::BuildServerConfig {
                        path: cert_path.clone(),
                        source,
                    })?,
            ))
        } else {
            None
        };

        let client = {
            let base = ClientConfig::builder();
            let base = match &anchors {
                Some((_, store)) => base.with_root_certificates(store.clone()),
                None => base.with_root_certificates(RootCertStore::empty()),
            };
            let mut client = if let Some((cert_path, certs, key)) = &identity {
                // Presented only when a dialed peer requests client
                // authentication; a no-op against peers that do not.
                base.with_client_auth_cert(certs.clone(), key.clone_key())
                    .map_err(|source| Error::BuildClientConfig {
                        path: cert_path.clone(),
                        source,
                    })?
            } else {
                base.with_no_client_auth()
            };
            if self.insecure_skip_verify {
                warn!("TLS client: accepting any peer certificate without validation (INSECURE)");
                let verifier = InsecureVerifier::new(client.crypto_provider().clone());
                client
                    .dangerous()
                    .set_certificate_verifier(Arc::new(verifier));
            }
            client
        };

        Ok(Tls {
            required: self.required,
            server,
            client: Arc::new(client),
            server_name: self.server_name,
        })
    }

    // Scan `dir` for PEM certificates, pairing each with the file it came
    // from so a later trust-store rejection stays attributable. Reports
    // filesystem problems with the directory; the caller decides what an
    // empty result and trust-store rejections mean.
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

        debug!("Loading CA certificates from directory: {}", dir.display());

        let entries = fs::read_dir(dir).map_err(|source| Error::ReadCaCerts {
            path: dir.to_path_buf(),
            source,
        })?;

        let mut certs = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| Error::ReadCaCerts {
                path: dir.to_path_buf(),
                source,
            })?;

            let file_path = entry.path();

            if file_path.is_dir() {
                continue;
            }

            // Try to parse certificates - skip files that cannot be read or
            // parsed, they might be other files like .srl, .key, .csr, etc.
            let parsed = match CertificateDer::pem_file_iter(&file_path)
                .and_then(|iter| iter.collect::<std::result::Result<Vec<_>, _>>())
            {
                Ok(parsed) => parsed,
                Err(e) => {
                    debug!(
                        "Skipping file {} (not a valid certificate file: {e})",
                        file_path.display()
                    );
                    continue;
                }
            };

            certs.extend(parsed.into_iter().map(|cert| (file_path.clone(), cert)));
        }

        Ok(certs)
    }
}
