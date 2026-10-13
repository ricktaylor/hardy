// The certificate verifiers for both handshake roles: RFC 5280 path
// validation to the trust anchors, then Node ID authentication from the
// end-entity certificate (Section 4.2). Network-level identifiers are not
// checked: a peer is identified by its Node ID alone, the policy RFC 9174
// Section 4.4.5 recommends.

use std::sync::Arc;

use hardy_bpv7::eid::NodeId;
use rustls::{
    CertificateError, DigitallySignedStruct, DistinguishedName, RootCertStore, SignatureScheme,
    client::{
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
        verify_server_cert_signed_by_trust_anchor,
    },
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime},
    server::{
        ParsedCertificate,
        danger::{ClientCertVerified, ClientCertVerifier},
    },
};
use tracing::warn;

use crate::node_id::{self, authenticate};

// Fails the handshake with a bad_certificate alert, as RFC 9174 Section
// 4.4.4.1 requires: rustls sends that alert for a certificate not valid for
// the expected identity, which is what a Node ID failure is.
fn node_id_failure(error: node_id::Error) -> rustls::Error {
    warn!("refusing the peer's certificate: {error}");
    rustls::Error::InvalidCertificate(match error {
        node_id::Error::Certificate(_) => CertificateError::BadEncoding,
        node_id::Error::Absent | node_id::Error::Mismatch { .. } => {
            CertificateError::NotValidForName
        }
    })
}

/// The dialing side's verifier of the accepting peer's certificate, holding the Node ID configured for that peer, if any.
#[derive(Debug)]
pub struct ServerVerifier {
    roots: Arc<RootCertStore>,
    provider: Arc<CryptoProvider>,
    configured: Option<NodeId>,
}

impl ServerVerifier {
    pub fn new(
        roots: Arc<RootCertStore>,
        provider: Arc<CryptoProvider>,
        configured: Option<NodeId>,
    ) -> Self {
        Self {
            roots,
            provider,
            configured,
        }
    }
}

impl ServerCertVerifier for ServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let cert = ParsedCertificate::try_from(end_entity)?;
        verify_server_cert_signed_by_trust_anchor(
            &cert,
            &self.roots,
            intermediates,
            now,
            self.provider.signature_verification_algorithms.all,
        )?;
        authenticate(end_entity, self.configured.as_ref()).map_err(node_id_failure)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// The accepting side's verifier of the dialing peer's certificate: the WebPKI client verifier, which demands a certificate, followed by Node ID authentication.
#[derive(Debug)]
pub struct ClientVerifier(Arc<dyn ClientCertVerifier>);

impl ClientVerifier {
    pub fn new(webpki: Arc<dyn ClientCertVerifier>) -> Self {
        Self(webpki)
    }
}

impl ClientCertVerifier for ClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        self.0.root_hint_subjects()
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        let verified = self.0.verify_client_cert(end_entity, intermediates, now)?;
        authenticate(end_entity, None).map_err(node_id_failure)?;
        Ok(verified)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}
