// The QUIC endpoint: one UDP socket that both accepts and dials Qubicle
// sessions, each authenticated by mutual TLS and identified by the peer's
// Node IDs (Section 4.2).

use std::{io, net::SocketAddr, sync::Arc};

use hardy_bpv7::eid::NodeId;
use quinn::{
    ClientConfig,
    ConnectError,
    Connection,
    ConnectionError,
    // Aliased: this module's own type is `Endpoint`.
    Endpoint as QuicEndpoint,
    Incoming,
    ServerConfig,
    TransportConfig,
    VarInt,
};
use rustls::pki_types::CertificateDer;
use thiserror::Error;

use crate::{
    datagram::{self, DatagramReceiver, DatagramSender},
    error_code::ErrorCode,
    node_id::{self, authenticate},
    reliable::{self, BundleReceiver, BundleSender, var_int},
    tls::{self, Tls},
};

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to start a QUIC connection: {0}")]
    Connect(#[source] ConnectError),

    #[error("the QUIC connection failed: {0}")]
    Connection(#[source] ConnectionError),

    #[error("the peer presented no certificate")]
    NoPeerCertificate,

    #[error("failed to authenticate the peer's Node ID: {0}")]
    NodeId(#[source] node_id::Error),

    #[error(transparent)]
    Tls(#[from] tls::Error),
}

// Whether an endpoint offers the unreliable service (Section 4.4), by
// advertising max_datagram_frame_size. The service is available on a
// session only when both peers offer it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unreliable {
    Offered,
    NotOffered,
}

// The transport parameters common to every session. A peer may open no
// bidirectional streams (Section 4.3.3). Offering the unreliable service
// keeps quinn's default datagram receive buffer, whose presence advertises
// max_datagram_frame_size.
//
// Every UDP datagram carrying 1-RTT packets is padded to the path MTU, and
// path MTU discovery is off, so the MTU stays at the 1200 octets every QUIC
// path supports (RFC 9000 Section 14). An observer then sees datagrams of a
// single size, which hides bundle and transfer sizes from traffic analysis
// (Section 6). Initial and Handshake packets are sized by the handshake
// itself.
fn transport_config(unreliable: Unreliable) -> TransportConfig {
    let mut transport = TransportConfig::default();
    transport
        .max_concurrent_bidi_streams(VarInt::from_u32(0))
        .mtu_discovery_config(None)
        .pad_to_mtu(true);
    if unreliable == Unreliable::NotOffered {
        transport.datagram_receive_buffer_size(None);
    }
    transport
}

#[derive(Debug)]
pub struct Endpoint {
    endpoint: QuicEndpoint,
    tls: Arc<Tls>,
    transport: Arc<TransportConfig>,
    unreliable: Unreliable,
}

impl Endpoint {
    // Binds `addr`, accepting sessions as well as dialing them.
    pub fn new(addr: SocketAddr, tls: Arc<Tls>, unreliable: Unreliable) -> io::Result<Self> {
        let transport = Arc::new(transport_config(unreliable));
        let mut server = ServerConfig::with_crypto(tls.server_config());
        server.transport_config(transport.clone());
        let endpoint = QuicEndpoint::server(server, addr)?;
        Ok(Self {
            endpoint,
            tls,
            transport,
            unreliable,
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }

    // Dials the peer at `addr`. `configured` is the Node ID configured for
    // the peer, if any: the handshake fails unless the peer's certificate
    // carries it, or carries no NODE-ID at all (Section 4.2).
    //
    // The TLS server name is the peer's IP address. It is sent but never
    // checked: peers are identified by Node ID alone (RFC 9174 Section
    // 4.4.5).
    pub async fn connect(&self, addr: SocketAddr, configured: Option<NodeId>) -> Result<Session> {
        let mut client = ClientConfig::new(Arc::new(self.tls.client_config(configured.clone())?));
        client.transport_config(self.transport.clone());
        let connection = self
            .endpoint
            .connect_with(client, addr, &addr.ip().to_string())
            .map_err(Error::Connect)?
            .await
            .map_err(Error::Connection)?;
        Session::new(connection, configured.as_ref(), self.unreliable)
    }

    // The next session a peer dials, or `None` once the endpoint is closed.
    // The handshake runs in `Accepting::session`, so that one slow peer
    // does not hold up the others.
    pub async fn accept(&self) -> Option<Accepting> {
        let incoming = self.endpoint.accept().await?;
        Some(Accepting {
            incoming,
            unreliable: self.unreliable,
        })
    }

    // Closes every session at once with `code`, and stops accepting new
    // ones.
    pub fn close(&self, code: ErrorCode) {
        self.endpoint.close(var_int(code), b"");
    }
}

// A session a peer has begun to dial.
#[derive(Debug)]
pub struct Accepting {
    incoming: Incoming,
    unreliable: Unreliable,
}

impl Accepting {
    // Completes the handshake.
    pub async fn session(self) -> Result<Session> {
        match self.incoming.await {
            Ok(connection) => Session::new(connection, None, self.unreliable),
            Err(e) => Err(Error::Connection(e)),
        }
    }
}

// An established, authenticated session.
#[derive(Debug)]
pub struct Session {
    connection: Connection,
    peer_node_ids: Vec<NodeId>,
    unreliable: Unreliable,
}

impl Session {
    // The certificate verifier has already authenticated the peer during
    // the handshake; this reads back the Node IDs it accepted.
    fn new(
        connection: Connection,
        configured: Option<&NodeId>,
        unreliable: Unreliable,
    ) -> Result<Self> {
        let certs = connection
            .peer_identity()
            .and_then(|identity| identity.downcast::<Vec<CertificateDer<'static>>>().ok())
            .ok_or(Error::NoPeerCertificate)?;
        let end_entity = certs.first().ok_or(Error::NoPeerCertificate)?;
        let peer_node_ids = authenticate(end_entity, configured).map_err(Error::NodeId)?;
        Ok(Self {
            connection,
            peer_node_ids,
            unreliable,
        })
    }

    pub fn peer_node_ids(&self) -> &[NodeId] {
        &self.peer_node_ids
    }

    pub fn remote_address(&self) -> SocketAddr {
        self.connection.remote_address()
    }

    // Opens a stream for a bundle of `len` octets (Section 4.3), waiting for
    // stream credit from the peer.
    pub async fn open_bundle(&self, len: u64) -> reliable::Result<BundleSender> {
        reliable::open(&self.connection, len).await
    }

    // The next bundle the peer sends. Bundles that had fully arrived before
    // the connection closed are still returned after it closes; an error
    // means there are no more.
    pub async fn accept_bundle(&self) -> reliable::Result<BundleReceiver> {
        reliable::accept(&self.connection).await
    }

    // Closes the session at once with `code` (Section 4.5).
    pub fn close(&self, code: ErrorCode) {
        self.connection.close(var_int(code), b"");
    }

    // Waits for the session to close, returning why.
    pub async fn closed(&self) -> ConnectionError {
        self.connection.closed().await
    }

    pub fn is_closed(&self) -> bool {
        self.connection.close_reason().is_some()
    }

    #[cfg(test)]
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    // Opens the unreliable service, if both peers offered it. QUIC itself
    // would let this side send datagrams whenever the peer advertised
    // max_datagram_frame_size, but Section 4.4 requires both to have.
    pub fn open_datagrams(
        &self,
        config: datagram::Config,
    ) -> datagram::Result<(DatagramSender, DatagramReceiver)> {
        if self.unreliable == Unreliable::NotOffered {
            return Err(datagram::Error::Unavailable);
        }
        datagram::open(&self.connection, config)
    }
}

#[cfg(test)]
pub mod tests {
    //! Real QUIC handshakes over loopback between two endpoints, each with
    //! TLS material loaded from files written by a test CA. The CA and the
    //! handshake are shared with the tests of the services that run on a
    //! session.

    use std::{
        fs,
        net::{Ipv4Addr, SocketAddrV4},
        path::PathBuf,
    };

    use quinn::TransportErrorCode;
    use rcgen::{
        BasicConstraints, CertificateParams, CertifiedIssuer, CustomExtension, DistinguishedName,
        DnType, IsCa, KeyPair,
    };
    use rustls::AlertDescription;
    use tempfile::TempDir;

    use super::*;
    use crate::node_id::tests::bundle_eids;

    // A certificate authority, with its certificate alone in `ca/`, the
    // directory a node trusting it is configured with. Each CA needs its own
    // name: path building matches issuers by name, so two CAs sharing one
    // would turn an unknown issuer into a bad signature.
    pub struct Ca {
        dir: TempDir,
        issuer: CertifiedIssuer<'static, KeyPair>,
    }

    impl Ca {
        pub fn new(name: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let mut params = CertificateParams::new(Vec::new()).unwrap();
            params.distinguished_name = DistinguishedName::new();
            params.distinguished_name.push(DnType::CommonName, name);
            params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            let issuer =
                CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap();
            fs::create_dir(dir.path().join("ca")).unwrap();
            fs::write(dir.path().join("ca/ca.pem"), issuer.pem()).unwrap();
            Self { dir, issuer }
        }

        // Issues a certificate and key for `name`, with the subjectAltName
        // `san` or none, and writes both as PEM files.
        pub fn identity(&self, name: &str, san: Option<CustomExtension>) -> (PathBuf, PathBuf) {
            let mut params = CertificateParams::new(Vec::new()).unwrap();
            params.custom_extensions.extend(san);
            let key = KeyPair::generate().unwrap();
            let cert = params.signed_by(&key, &self.issuer).unwrap();
            let cert_path = self.dir.path().join(format!("{name}.pem"));
            let key_path = self.dir.path().join(format!("{name}.key"));
            fs::write(&cert_path, cert.pem()).unwrap();
            fs::write(&key_path, key.serialize_pem()).unwrap();
            (cert_path, key_path)
        }

        // TLS material trusting this CA, presenting `identity`.
        pub fn tls(&self, (cert, key): (PathBuf, PathBuf)) -> Tls {
            Tls::builder()
                .ca_certs(self.dir.path().join("ca"))
                .identity(cert, key)
                .build()
                .unwrap()
        }

        // An endpoint on an ephemeral loopback port, trusting this CA.
        pub fn endpoint(&self, identity: (PathBuf, PathBuf)) -> Endpoint {
            self.endpoint_offering(identity, Unreliable::Offered)
        }

        pub fn endpoint_offering(
            &self,
            identity: (PathBuf, PathBuf),
            unreliable: Unreliable,
        ) -> Endpoint {
            let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into();
            Endpoint::new(addr, Arc::new(self.tls(identity)), unreliable).unwrap()
        }
    }

    pub fn node_id(s: &str) -> NodeId {
        s.parse().unwrap()
    }

    // Dials `listener` from `dialer` and accepts on `listener`, returning
    // both outcomes once each side has finished its handshake.
    pub async fn handshake(
        dialer: &Endpoint,
        listener: &Endpoint,
        configured: Option<NodeId>,
    ) -> (Result<Session>, Result<Session>) {
        let addr = listener.local_addr().unwrap();
        let accepted = async {
            listener
                .accept()
                .await
                .expect("the listener is open")
                .session()
                .await
        };
        tokio::join!(dialer.connect(addr, configured), accepted)
    }

    // Two sessions joined by a handshake between endpoints that offer the
    // unreliable service as given: the dialed session, then the accepted.
    pub async fn sessions(dialer: Unreliable, listener: Unreliable) -> (Session, Session) {
        let ca = Ca::new("trusted");
        let dialer = ca.endpoint_offering(
            ca.identity("dialer", Some(bundle_eids(&["ipn:977000.1.0"]))),
            dialer,
        );
        let listener = ca.endpoint_offering(
            ca.identity("listener", Some(bundle_eids(&["ipn:977000.2.0"]))),
            listener,
        );
        let (dialed, accepted) = handshake(&dialer, &listener, None).await;
        (dialed.unwrap(), accepted.unwrap())
    }

    // The code of the connection close a TLS alert causes (RFC 9001
    // Section 4.8).
    fn alert(description: AlertDescription) -> TransportErrorCode {
        TransportErrorCode::crypto(description.into())
    }

    fn refused_locally(result: Result<Session>) -> TransportErrorCode {
        match result {
            Err(Error::Connection(ConnectionError::TransportError(e))) => e.code,
            other => panic!("expected a local transport error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn each_side_learns_the_node_ids_of_the_other() {
        let ca = Ca::new("trusted");
        let dialer = ca.endpoint(ca.identity("dialer", Some(bundle_eids(&["ipn:977000.1.0"]))));
        let listener = ca.endpoint(ca.identity(
            "listener",
            Some(bundle_eids(&["ipn:977000.2.0", "dtn://listener.example/"])),
        ));

        let (dialed, accepted) = handshake(&dialer, &listener, None).await;
        let (dialed, accepted) = (dialed.unwrap(), accepted.unwrap());
        assert_eq!(
            dialed.peer_node_ids(),
            [
                node_id("ipn:977000.2.0"),
                node_id("dtn://listener.example/")
            ]
        );
        assert_eq!(accepted.peer_node_ids(), [node_id("ipn:977000.1.0")]);
        assert_eq!(dialed.remote_address(), listener.local_addr().unwrap());
    }

    #[tokio::test]
    async fn a_configured_node_id_the_certificate_carries_is_accepted() {
        let ca = Ca::new("trusted");
        let dialer = ca.endpoint(ca.identity("dialer", Some(bundle_eids(&["ipn:977000.1.0"]))));
        let listener = ca.endpoint(ca.identity(
            "listener",
            Some(bundle_eids(&["ipn:977000.2.0", "ipn:977000.3.0"])),
        ));

        let (dialed, _) = handshake(&dialer, &listener, Some(node_id("ipn:977000.3.0"))).await;
        assert_eq!(
            dialed.unwrap().peer_node_ids(),
            [node_id("ipn:977000.2.0"), node_id("ipn:977000.3.0")]
        );
    }

    // Section 4.2: a mismatch is a certificate validation failure that
    // terminates the handshake, with the bad_certificate alert RFC 9174
    // Section 4.4.4.1 specifies.
    #[tokio::test]
    async fn a_configured_node_id_the_certificate_lacks_fails_the_handshake() {
        let ca = Ca::new("trusted");
        let dialer = ca.endpoint(ca.identity("dialer", Some(bundle_eids(&["ipn:977000.1.0"]))));
        let listener = ca.endpoint(ca.identity("listener", Some(bundle_eids(&["ipn:977000.2.0"]))));

        let (dialed, _) = handshake(&dialer, &listener, Some(node_id("ipn:977000.3.0"))).await;
        assert_eq!(
            refused_locally(dialed),
            alert(AlertDescription::BadCertificate)
        );
    }

    #[tokio::test]
    async fn a_configured_node_id_stands_in_for_an_absent_node_id() {
        let ca = Ca::new("trusted");
        let dialer = ca.endpoint(ca.identity("dialer", Some(bundle_eids(&["ipn:977000.1.0"]))));
        let listener = ca.endpoint(ca.identity("listener", None));

        let (dialed, _) = handshake(&dialer, &listener, Some(node_id("ipn:977000.2.0"))).await;
        assert_eq!(dialed.unwrap().peer_node_ids(), [node_id("ipn:977000.2.0")]);
    }

    #[tokio::test]
    async fn the_dialer_refuses_a_listener_without_a_node_id() {
        let ca = Ca::new("trusted");
        let dialer = ca.endpoint(ca.identity("dialer", Some(bundle_eids(&["ipn:977000.1.0"]))));
        let listener = ca.endpoint(ca.identity("listener", None));

        let (dialed, _) = handshake(&dialer, &listener, None).await;
        assert_eq!(
            refused_locally(dialed),
            alert(AlertDescription::BadCertificate)
        );
    }

    // An endpoint ID that is not a node ID authenticates nothing, so the
    // certificate is treated as having no NODE-ID.
    #[tokio::test]
    async fn the_listener_refuses_a_dialer_without_a_node_id() {
        let ca = Ca::new("trusted");
        let dialer = ca.endpoint(ca.identity(
            "dialer",
            Some(bundle_eids(&["dtn://dialer.example/service"])),
        ));
        let listener = ca.endpoint(ca.identity("listener", Some(bundle_eids(&["ipn:977000.2.0"]))));

        let (_, accepted) = handshake(&dialer, &listener, None).await;
        assert_eq!(
            refused_locally(accepted),
            alert(AlertDescription::BadCertificate)
        );
    }

    // The accepting side discards its Handshake keys when its handshake
    // completes (RFC 9001 Section 4.9.2), so everything it sends once
    // `accept` returns is a 1-RTT packet, each padded into a datagram of
    // the 1200-octet minimum MTU.
    #[tokio::test]
    async fn every_datagram_after_the_handshake_is_padded_to_1200_octets() {
        const MIN_MTU: u64 = 1200;

        let ca = Ca::new("trusted");
        let dialer = ca.endpoint(ca.identity("dialer", Some(bundle_eids(&["ipn:977000.1.0"]))));
        let listener = ca.endpoint(ca.identity("listener", Some(bundle_eids(&["ipn:977000.2.0"]))));
        let (dialed, accepted) = handshake(&dialer, &listener, None).await;
        let (dialed, accepted) = (dialed.unwrap(), accepted.unwrap());
        let before = accepted.connection.stats().udp_tx;

        // One short write and one spanning several packets, so that both
        // a short packet and full ones would show up unpadded.
        let data: Vec<u8> = (0..4000u32).map(|i| i as u8).collect();
        let mut send = accepted.connection.open_uni().await.unwrap();
        send.write_all(&data[..10]).await.unwrap();
        send.write_all(&data[10..]).await.unwrap();
        send.finish().unwrap();
        let mut recv = dialed.connection.accept_uni().await.unwrap();
        assert_eq!(recv.read_to_end(data.len()).await.unwrap(), data);
        // The dialer's acknowledgement of the FIN has reached the
        // listener, which answers nothing further in between.
        send.stopped().await.unwrap();

        let after = accepted.connection.stats().udp_tx;
        let datagrams = after.datagrams - before.datagrams;
        assert!(datagrams >= 4, "only {datagrams} datagrams were sent");
        assert_eq!(after.bytes - before.bytes, datagrams * MIN_MTU);
    }

    #[tokio::test]
    async fn a_certificate_from_an_untrusted_ca_is_refused() {
        let ca = Ca::new("trusted");
        let other = Ca::new("untrusted");
        let dialer = ca.endpoint(ca.identity("dialer", Some(bundle_eids(&["ipn:977000.1.0"]))));
        let listener =
            ca.endpoint(other.identity("listener", Some(bundle_eids(&["ipn:977000.2.0"]))));

        let (dialed, _) = handshake(&dialer, &listener, None).await;
        assert_eq!(refused_locally(dialed), alert(AlertDescription::UnknownCA));
    }
}
