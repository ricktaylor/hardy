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

#[cfg(test)]
mod tests {
    //! Real mutual-TLS handshakes over an in-memory duplex, driving the
    //! loaded material through its own `acceptor()`/`connector()`. The
    //! session machinery (contact headers, SESS_INIT) is tested
    //! elsewhere; these isolate the one question `client-auth` answers:
    //! does the listener request and verify dialers' certificates, per
    //! policy.

    use std::path::{Path, PathBuf};

    use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
    use rustls::pki_types::ServerName;

    use super::{ClientAuth, Tls};

    const SERVER_NAME: &str = "peer.dtn.example.com";

    // A throwaway PKI in a tempdir: a CA, and leaf certificates it signs
    // on demand. The trust anchor is a directory of PEM files, matching
    // what the builder loads; each identity is a cert+key pair.
    struct Pki {
        dir: tempfile::TempDir,
        issuer: Issuer<'static, KeyPair>,
    }

    impl Pki {
        fn new(name: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let ca_key = KeyPair::generate().unwrap();
            let mut params = CertificateParams::new(Vec::new()).unwrap();
            params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            let ca = params.self_signed(&ca_key).unwrap();

            let ca_dir = dir.path().join(format!("{name}-ca"));
            std::fs::create_dir(&ca_dir).unwrap();
            std::fs::write(ca_dir.join("ca.pem"), ca.pem()).unwrap();

            Self {
                dir,
                issuer: Issuer::new(params, ca_key),
            }
        }

        fn ca_certs(&self, name: &str) -> PathBuf {
            self.dir.path().join(format!("{name}-ca"))
        }

        // Mint a leaf signed by this CA; returns its (cert, key) paths.
        fn identity(&self, name: &str, san: &str) -> (PathBuf, PathBuf) {
            let key = KeyPair::generate().unwrap();
            let params = CertificateParams::new(vec![san.to_string()]).unwrap();
            let cert = params.signed_by(&key, &self.issuer).unwrap();

            let cert_path = self.dir.path().join(format!("{name}.crt"));
            let key_path = self.dir.path().join(format!("{name}.key"));
            std::fs::write(&cert_path, cert.pem()).unwrap();
            std::fs::write(&key_path, key.serialize_pem()).unwrap();
            (cert_path, key_path)
        }
    }

    // Whether a session is established between `client` dialing and
    // `server` accepting over an in-memory duplex: `Ok` means both sides
    // completed the handshake, an `Err` from either (a refused client
    // certificate, an untrusted server) fails it.
    async fn handshakes(server: &Tls, client: &Tls) -> Result<(), String> {
        let (client_io, server_io) = tokio::io::duplex(16 * 1024);
        let acceptor = server.acceptor().expect("server material has an identity");
        let connector = client.connector();
        let server_name = ServerName::try_from(SERVER_NAME).unwrap();

        let (accepted, connected) = tokio::join!(
            acceptor.accept(server_io),
            connector.connect(server_name, client_io),
        );
        accepted.map_err(|e| format!("accept: {e}"))?;
        connected.map_err(|e| format!("connect: {e}"))?;
        Ok(())
    }

    // Server material with an identity, trusting `ca_dir`, at `policy`.
    fn server(ca_dir: &Path, cert: &Path, key: &Path, policy: ClientAuth) -> Tls {
        Tls::builder()
            .ca_certs(ca_dir.to_path_buf())
            .identity(cert.to_path_buf(), key.to_path_buf())
            .client_auth(policy)
            .build()
            .unwrap()
    }

    // A verifying dialer that presents no certificate.
    fn client_without_identity(ca_dir: &Path) -> Tls {
        Tls::builder()
            .ca_certs(ca_dir.to_path_buf())
            .build()
            .unwrap()
    }

    // A dialer that also presents its own identity for mutual TLS.
    fn client_with_identity(ca_dir: &Path, cert: &Path, key: &Path) -> Tls {
        Tls::builder()
            .ca_certs(ca_dir.to_path_buf())
            .identity(cert.to_path_buf(), key.to_path_buf())
            .build()
            .unwrap()
    }

    // Baseline: with client-auth off, any trusting dialer completes,
    // certificate or not.
    #[tokio::test]
    async fn client_auth_off_accepts_any_trusting_dialer() {
        let pki = Pki::new("p");
        let (sc, sk) = pki.identity("server", SERVER_NAME);
        let ca = pki.ca_certs("p");

        let server = server(&ca, &sc, &sk, ClientAuth::Off);
        assert!(
            handshakes(&server, &client_without_identity(&ca))
                .await
                .is_ok(),
            "certless dialer must be accepted when client-auth is off"
        );
    }

    // `optional` verifies a presented certificate but admits a dialer
    // without one: the fleet-migration step.
    #[tokio::test]
    async fn client_auth_optional_admits_certless_and_verifies_presented() {
        let pki = Pki::new("p");
        let (sc, sk) = pki.identity("server", SERVER_NAME);
        let (cc, ck) = pki.identity("client", "client.dtn.example.com");
        let ca = pki.ca_certs("p");

        assert!(
            handshakes(
                &server(&ca, &sc, &sk, ClientAuth::Optional),
                &client_without_identity(&ca),
            )
            .await
            .is_ok(),
            "certless dialer must be admitted under optional"
        );
        assert!(
            handshakes(
                &server(&ca, &sc, &sk, ClientAuth::Optional),
                &client_with_identity(&ca, &cc, &ck),
            )
            .await
            .is_ok(),
            "a valid client certificate must verify under optional"
        );
    }

    // `required` refuses a certless dialer and accepts one whose
    // certificate chains to the trusted CA.
    #[tokio::test]
    async fn client_auth_required_refuses_certless_accepts_valid() {
        let pki = Pki::new("p");
        let (sc, sk) = pki.identity("server", SERVER_NAME);
        let (cc, ck) = pki.identity("client", "client.dtn.example.com");
        let ca = pki.ca_certs("p");

        assert!(
            handshakes(
                &server(&ca, &sc, &sk, ClientAuth::Required),
                &client_without_identity(&ca),
            )
            .await
            .is_err(),
            "a certless dialer must be refused under required"
        );
        assert!(
            handshakes(
                &server(&ca, &sc, &sk, ClientAuth::Required),
                &client_with_identity(&ca, &cc, &ck),
            )
            .await
            .is_ok(),
            "a CA-chained client certificate must be accepted under required"
        );
    }

    // A client certificate from a different CA is refused: presenting a
    // certificate means it must verify, under optional and required alike.
    #[tokio::test]
    async fn client_auth_rejects_untrusted_client_ca() {
        let server_pki = Pki::new("server");
        let other_pki = Pki::new("other");
        let (sc, sk) = server_pki.identity("server", SERVER_NAME);
        let server_ca = server_pki.ca_certs("server");

        // The dialer trusts the server's CA (so it accepts the server)
        // but presents an identity minted by an unrelated CA.
        let (cc, ck) = other_pki.identity("client", "client.dtn.example.com");
        let rogue = Tls::builder()
            .ca_certs(server_ca.clone())
            .identity(cc, ck)
            .build()
            .unwrap();

        for policy in [ClientAuth::Optional, ClientAuth::Required] {
            assert!(
                handshakes(&server(&server_ca, &sc, &sk, policy), &rogue)
                    .await
                    .is_err(),
                "a client certificate from an untrusted CA must be refused under {policy:?}"
            );
        }
    }

    // The dialing side still rejects a server outside its trust anchor:
    // the base (non-mutual) guarantee holds.
    #[tokio::test]
    async fn dialer_rejects_untrusted_server() {
        let server_pki = Pki::new("server");
        let other_pki = Pki::new("other");
        let (sc, sk) = server_pki.identity("server", SERVER_NAME);

        let server = server(&server_pki.ca_certs("server"), &sc, &sk, ClientAuth::Off);
        let client = client_without_identity(&other_pki.ca_certs("other"));
        assert!(
            handshakes(&server, &client).await.is_err(),
            "a server certificate outside the dialer's trust anchor must be refused"
        );
    }
}
