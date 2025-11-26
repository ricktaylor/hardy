use rustls_pemfile::{certs, pkcs8_private_keys};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use thiserror::Error;

use rustls::DigitallySignedStruct;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, RootCertStore, ServerConfig, SignatureScheme};
use tracing::{debug, info, warn};

#[derive(Error, Debug)]
pub enum TlsError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("TLS error: {0}")]
    Rustls(#[from] rustls::Error),

    #[error("{0}")]
    CertificateLoad(String),

    #[error("{0}")]
    PrivateKeyLoad(String),
}


pub struct TlsConfig {
    pub server_config: Option<Arc<ServerConfig>>,
    pub client_config: Arc<ClientConfig>,
    pub server_name: Option<String>,
}

impl TlsConfig {
    pub fn new(config: &super::config::TlsConfig) -> Result<Self, TlsError> {
        let server_config = Self::build_server_config(config)?;
        let client_config = Self::build_client_config(config)?;

        Ok(Self {
            server_config,
            client_config: Arc::new(client_config),
            server_name: config.server_name.clone(),
        })
    }

    fn build_server_config(
        config: &super::config::TlsConfig,
    ) -> Result<Option<Arc<ServerConfig>>, TlsError> {
        match (&config.server_cert, &config.server_key) {
            (Some(cert_path), Some(key_path)) => {
                let certs = load_certs(cert_path)?;
                let key = load_private_key(key_path)?;

                let server_config = ServerConfig::builder()
                    .with_no_client_auth()
                    .with_single_cert(certs, key)
                    .map_err(|e| {
                        TlsError::CertificateLoad(format!(
                            "Server TLS configuration error for {}: {e}",
                            cert_path.display()
                        ))
                    })?;

                Ok(Some(Arc::new(server_config)))
            }
            (Some(_), None) | (None, Some(_)) => Err(TlsError::CertificateLoad(
                "Both server_cert and server_key must be provided together".to_string(),
            )),
            (None, None) => Ok(None),
        }
    }

    fn build_client_config(config: &super::config::TlsConfig) -> Result<ClientConfig, TlsError> {
        let mut root_store = RootCertStore::empty();

        // Load CA certificates from bundle directory if provided
        if let Some(ca_bundle) = &config.ca_bundle {
            load_ca_certs(&mut root_store, ca_bundle)?;
            info!(
                "Successfully loaded CA certificates from bundle (total in store: {})",
                root_store.len()
            );
        }

        // Build client config with appropriate certificate verification
        if config.debug.accept_self_signed {
            warn!(
                "TLS client: Using custom verifier to accept self-signed certificates (INSECURE)"
            );
            let mut client_config = ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth();
            client_config
                .dangerous()
                .set_certificate_verifier(Arc::new(SelfSignedVerifier));
            Ok(client_config)
        } else {
            if root_store.is_empty() {
                return Err(TlsError::CertificateLoad(
                    "TLS CA store is empty and accept_self_signed is disabled. \
                    Configure a CA bundle directory or enable accept_self_signed for testing only"
                        .to_string(),
                ));
            }
            Ok(ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth())
        }
    }
}

fn resolve_path(path: &std::path::Path) -> Result<std::path::PathBuf, TlsError> {
    if path.as_os_str().is_empty() {
        return Err(TlsError::CertificateLoad("Path is empty".to_string()));
    }
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()
            .map_err(|e| {
                TlsError::CertificateLoad(format!(
                    "Cannot get current directory to resolve path {}: {e}",
                    path.display()
                ))
            })?
            .join(path))
    }
}

fn read_file(path: &Path, label: &str) -> Result<Vec<u8>, TlsError> {
    let data = fs::read(path).map_err(|e| {
        TlsError::CertificateLoad(format!("Cannot read {label} from {}: {e}", path.display()))
    })?;

    if data.is_empty() {
        return Err(TlsError::CertificateLoad(format!(
            "{label} file is empty: {}",
            path.display()
        )));
    }

    Ok(data)
}

fn load_certs(path: &std::path::Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let resolved = resolve_path(path)?;
    let data = read_file(&resolved, "Certificate")?;

    certs(&mut data.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| {
            TlsError::CertificateLoad(format!(
                "Cannot parse certificate from {}: {e}",
                resolved.display()
            ))
        })
}

fn load_private_key(path: &std::path::Path) -> Result<PrivateKeyDer<'static>, TlsError> {
    let resolved = resolve_path(path)?;
    let data = read_file(&resolved, "Private key")?;

    // Try to load private key in PKCS8 format
    if let Ok(mut keys) = pkcs8_private_keys(&mut data.as_slice()).collect::<Result<Vec<_>, _>>() {
        if !keys.is_empty() {
            return Ok(PrivateKeyDer::Pkcs8(keys.remove(0).clone_key()));
        }
    }

    // Question: Should we add RSA format as a fallback?
    // if let Ok(mut keys) = rsa_private_keys(&mut data.as_slice()).collect::<Result<Vec<_>, _>>() {
    //     if !keys.is_empty() {
    //         return Ok(PrivateKeyDer::Pkcs1(keys.remove(0).clone_key()));
    //     }
    // }

    Err(TlsError::PrivateKeyLoad(format!(
        "No private keys found in {} (tried PKCS8 format)",
        resolved.display()
    )))
}

fn load_ca_certs(store: &mut RootCertStore, path: &std::path::Path) -> Result<(), TlsError> {
    let resolved = resolve_path(path)?;

    if !resolved.exists() {
        return Err(TlsError::CertificateLoad(format!(
            "CA bundle directory does not exist: {}",
            resolved.display()
        )));
    }

    if !resolved.is_dir() {
        return Err(TlsError::CertificateLoad(format!(
            "CA bundle path must be a directory, not a file: {}",
            resolved.display()
        )));
    }

    let initial_len = store.len();
    debug!(
        "Loading CA certificates from directory: {}",
        resolved.display()
    );

    let entries = fs::read_dir(&resolved).map_err(|e| {
        TlsError::CertificateLoad(format!(
            "Cannot read CA bundle directory {}: {e}",
            resolved.display()
        ))
    })?;

    for entry in entries {
        let entry = entry.map_err(|e| {
            TlsError::CertificateLoad(format!(
                "Cannot read directory entry in {}: {e}",
                resolved.display()
            ))
        })?;

        let file_path = entry.path();

        if file_path.is_dir() {
            continue; // Skip directories
        }

        // Try to load certificates from this file
        // Ignore files that cannot be read or parsed (they might not be certificate files)
        let data = match fs::read(&file_path) {
            Ok(data) => data,
            Err(e) => {
                debug!("Skipping file {} (cannot read: {e})", file_path.display());
                continue;
            }
        };

        if data.is_empty() {
            continue;
        }

        // Try to parse certificates - ignore files that cannot be parsed
        let certs = match load_certs_from_file_data(&data, &file_path) {
            Ok(certs) => certs,
            Err(e) => {
                debug!(
                    "Skipping file {} (not a valid certificate file: {e})",
                    file_path.display()
                );
                continue;
            }
        };

        if certs.is_empty() {
            continue; // Skip files with no certificates silently
        }

        // Add all certificates to the store - fail if any cannot be added
        // (this indicates a real problem with a valid certificate)
        for cert in certs {
            store.add(cert).map_err(|e| {
                TlsError::CertificateLoad(format!(
                    "Cannot add CA certificate from {} to trust store: {e}",
                    file_path.display()
                ))
            })?;
        }
    }

    let loaded_count = store.len() - initial_len;

    if loaded_count == 0 {
        return Err(TlsError::CertificateLoad(format!(
            "No certificates found in CA bundle directory: {}",
            resolved.display(),
        )));
    }

    Ok(())
}

// Attempts to load certificates from file data.
fn load_certs_from_file_data(
    data: &[u8],
    file_path: &std::path::Path,
) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    certs(&mut &*data)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| {
            TlsError::CertificateLoad(format!(
                "Cannot parse certificates from {}: {e}",
                file_path.display()
            ))
        })
}

// Accepts all certificates (for testing with self-signed certs only)
#[derive(Debug)]
struct SelfSignedVerifier;

impl ServerCertVerifier for SelfSignedVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
            SignatureScheme::ED448,
        ]
    }
}
