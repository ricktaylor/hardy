// Construction of [`Tls`]: certificate and key loading via rustls's own
// PEM API and CA-bundle directory scanning. The builder owns its inputs
// directly; mapping from the user-facing configuration happens at the
// crate surface.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::{
    ClientConfig, RootCertStore, ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
};
use tracing::{debug, info, warn};

use super::{Error, Result, Tls, Trust, verifier::InsecureVerifier};

// Builder for [`Tls`]. Obtain one from [`Tls::builder`]; chain the inputs
// that apply, then `build()`.
#[must_use = "a TlsBuilder does nothing unless `build()` is called"]
pub struct TlsBuilder {
    trust: Trust,
    server: Option<(PathBuf, PathBuf)>,
    server_name: Option<String>,
}

impl TlsBuilder {
    // The client role's trust anchor is mandatory; everything else chains.
    pub(super) fn new(trust: Trust) -> Self {
        Self {
            trust,
            server: None,
            server_name: None,
        }
    }

    // Enable the server role: the PEM certificate presented to peers and
    // its private key (PKCS#8, PKCS#1, or SEC1). Taking both halves in one
    // call makes a lone certificate or key unrepresentable.
    pub fn server(mut self, cert_file: PathBuf, key_file: PathBuf) -> Self {
        self.server = Some((cert_file, key_file));
        self
    }

    // Override the SNI name presented when dialing (for certificates
    // issued to domain names).
    pub fn server_name(mut self, name: String) -> Self {
        self.server_name = Some(name);
        self
    }

    // Load and validate the TLS material described by the chained inputs.
    pub fn build(self) -> Result<Tls> {
        let server = if let Some((cert_path, key_path)) = &self.server {
            let certs = CertificateDer::pem_file_iter(cert_path)
                .and_then(|iter| iter.collect::<std::result::Result<Vec<_>, _>>())
                .map_err(|source| Error::LoadCertificate {
                    path: cert_path.clone(),
                    source,
                })?;
            let key =
                PrivateKeyDer::from_pem_file(key_path).map_err(|source| Error::LoadPrivateKey {
                    path: key_path.clone(),
                    source,
                })?;

            let server_config = ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(certs, key)
                .map_err(|source| Error::BuildServerConfig {
                    path: cert_path.clone(),
                    source,
                })?;

            Some(Arc::new(server_config))
        } else {
            None
        };

        let client = match &self.trust {
            Trust::CaBundle(dir) => {
                let certs = Self::load_ca_certs(dir)?;
                if certs.is_empty() {
                    return Err(Error::CaBundleEmpty { path: dir.clone() });
                }

                let mut store = RootCertStore::empty();
                for (file, cert) in certs {
                    store
                        .add(cert)
                        .map_err(|source| Error::AddTrustAnchor { path: file, source })?;
                }
                info!(
                    "Successfully loaded CA certificate(s) from bundle (total in store: {})",
                    store.len()
                );
                ClientConfig::builder()
                    .with_root_certificates(store)
                    .with_no_client_auth()
            }
            Trust::Insecure => {
                warn!("TLS client: accepting any peer certificate without validation (INSECURE)");
                let mut client_config = ClientConfig::builder()
                    .with_root_certificates(RootCertStore::empty())
                    .with_no_client_auth();
                let verifier = InsecureVerifier::new(client_config.crypto_provider().clone());
                client_config
                    .dangerous()
                    .set_certificate_verifier(Arc::new(verifier));
                client_config
            }
        };

        Ok(Tls {
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
            return Err(Error::CaBundleMissing {
                path: dir.to_path_buf(),
            });
        }

        if !dir.is_dir() {
            return Err(Error::CaBundleNotADirectory {
                path: dir.to_path_buf(),
            });
        }

        debug!("Loading CA certificates from directory: {}", dir.display());

        let entries = fs::read_dir(dir).map_err(|source| Error::ReadCaBundle {
            path: dir.to_path_buf(),
            source,
        })?;

        let mut certs = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| Error::ReadCaBundle {
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
