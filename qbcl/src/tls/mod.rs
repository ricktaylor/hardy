//! TLS material for the CLA: the node's identity, the trust anchors that validate peers, and the QUIC handshake configurations built from them. Construction chains from [`Tls::builder`].
//!
//! Both roles authenticate the peer by the Node ID its certificate carries (Section 4.2) after validating the certificate chain; host names and IP addresses in certificates are not checked. Session resumption is disabled in both roles, because a resumed handshake skips certificate validation and so would skip Node ID authentication.

use core::fmt;
use std::sync::Arc;

use hardy_bpv7::eid::NodeId;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::{
    ClientConfig, RootCertStore,
    client::Resumption,
    crypto::CryptoProvider,
    sign::{CertifiedKey, SingleCertAndKey},
    version::TLS13,
};

use crate::ALPN;

mod builder;
mod error;
mod verifier;

use self::verifier::ServerVerifier;
pub use self::{
    builder::TlsBuilder,
    error::{Error, Result},
};

/// The loaded TLS material. One identity serves both handshake roles.
pub struct Tls {
    provider: Arc<CryptoProvider>,
    roots: Arc<RootCertStore>,
    identity: Arc<CertifiedKey>,
    server: Arc<QuicServerConfig>,
}

impl Tls {
    /// Start building TLS material: chain [`TlsBuilder::ca_certs`] and [`TlsBuilder::identity`], both mandatory, then [`TlsBuilder::build`].
    pub fn builder() -> TlsBuilder {
        TlsBuilder::default()
    }

    // The configuration for accepting sessions. The accepting side has no
    // configured Node IDs: a dialing peer is authenticated by its
    // certificate's NODE-IDs alone.
    pub(crate) fn server_config(&self) -> Arc<QuicServerConfig> {
        self.server.clone()
    }

    // The configuration for dialing one peer: `configured` is the Node ID
    // configured for it, if any, which the peer's certificate must then
    // carry, or stand in for an absent NODE-ID (Section 4.2).
    pub(crate) fn client_config(&self, configured: Option<NodeId>) -> Result<QuicClientConfig> {
        let mut client = ClientConfig::builder_with_provider(self.provider.clone())
            .with_protocol_versions(&[&TLS13])
            .map_err(Error::Config)?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(ServerVerifier::new(
                self.roots.clone(),
                self.provider.clone(),
                configured,
            )))
            .with_client_cert_resolver(Arc::new(SingleCertAndKey::from(self.identity.clone())));
        client.alpn_protocols = vec![ALPN.to_vec()];
        client.resumption = Resumption::disabled();
        Ok(QuicClientConfig::try_from(client)?)
    }
}

// Written by hand so that no field, in particular the private key, can reach
// the output.
impl fmt::Debug for Tls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tls")
            .field("trust_anchors", &self.roots.len())
            .field("certificate_chain", &self.identity.cert.len())
            .finish_non_exhaustive()
    }
}
