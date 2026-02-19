use super::*;
use std::net::SocketAddr;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc::*,
};
use tokio_rustls::TlsAcceptor;

/// Data needed to handle a connection, without the TaskPool to avoid circular references.
///
/// This struct is shared between active (Connector) and passive (Listener) connection handling.
/// It contains all the configuration and state needed to negotiate and run a TCPCLv4 session,
/// but excludes the TaskPool to prevent Arc cycles when spawning tasks.
#[derive(Clone)]
pub struct ConnectionContext {
    pub session: config::SessionConfig,
    pub segment_mru: u64,
    pub transfer_mru: u64,
    pub node_ids: Arc<[NodeId]>,
    pub sink: Arc<dyn hardy_bpa::cla::Sink>,
    pub registry: Arc<connection::ConnectionRegistry>,
    pub tls_config: Option<Arc<tls::TlsConfig>>,
    pub session_cancel_token: tokio_util::sync::CancellationToken,
    pub task_cancel_token: hardy_async::CancellationToken,
}

impl std::fmt::Debug for ConnectionContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionContext")
            .field("session", &self.session)
            .field("segment_mru", &self.segment_mru)
            .field("transfer_mru", &self.transfer_mru)
            .field("node_ids", &self.node_ids)
            .field("tls_config", &self.tls_config)
            .finish_non_exhaustive()
    }
}

impl ConnectionContext {
    /// Returns the contact timeout as a Duration.
    pub fn contact_timeout_duration(&self) -> tokio::time::Duration {
        tokio::time::Duration::from_secs(self.session.contact_timeout as u64)
    }

    /// Returns the TLS flag byte for the contact header (1 if TLS configured, 0 otherwise).
    pub fn tls_contact_flag(&self) -> u8 {
        if self.tls_config.is_some() { 1 } else { 0 }
    }

    /// Returns the keepalive interval in seconds, defaulting to 0 if not configured.
    pub fn keepalive_interval_secs(&self) -> u16 {
        self.session.keepalive_interval.unwrap_or(0)
    }

    /// Get the first configured node ID.
    pub fn first_node_id(&self) -> Option<NodeId> {
        self.node_ids.first().cloned()
    }

    /// Negotiate keepalive interval with peer per RFC9174 Section 5.1.1.
    /// Returns the minimum of our interval and peer's interval, or 0 if we have none configured.
    pub fn negotiate_keepalive(&self, peer_keepalive: u16) -> u16 {
        self.session
            .keepalive_interval
            .map(|our_keepalive| peer_keepalive.min(our_keepalive))
            .unwrap_or(0)
    }

    /// Convert a keepalive interval (in seconds) to an Option<Duration>.
    /// Returns None if the interval is 0 (keepalive disabled).
    pub fn keepalive_as_duration(interval_secs: u16) -> Option<tokio::time::Duration> {
        if interval_secs != 0 {
            Some(tokio::time::Duration::from_secs(interval_secs as u64))
        } else {
            None
        }
    }

    /// Handle a new incoming contact (passive/server side).
    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    pub async fn new_contact(self, mut stream: TcpStream, remote_addr: SocketAddr) {
        // Receive contact header
        let mut buffer = [0u8; 6];
        match tokio::time::timeout(
            self.contact_timeout_duration(),
            stream.read_exact(&mut buffer),
        )
        .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                debug!("Read failed: {e}");
                return;
            }
            Err(_) => {
                debug!("Connection timed out");
                return;
            }
        }

        // Parse contact header
        if buffer[0..4] != *b"dtn!" {
            debug!("Contact header isn't: 'dtn!'");
            return;
        }

        debug!("Contact header received from {remote_addr}");

        // Always send our contact header in reply!
        if let Err(e) = stream
            .write_all(&[b'd', b't', b'n', b'!', 4, self.tls_contact_flag()])
            .await
        {
            debug!("Failed to send contact header: {e}");
            return;
        }

        if buffer[4] != 4 {
            warn!("Unsupported protocol version {}", buffer[4]);

            // Terminate session
            return transport::terminate(
                codec::MessageCodec::new_framed(stream),
                codec::SessionTermReasonCode::VersionMismatch,
                self.session.contact_timeout,
                &self.task_cancel_token,
            )
            .await;
        }

        if buffer[5] & 0xFE != 0 {
            info!(
                "Reserved flags {:#x} set in contact header from {remote_addr}",
                buffer[5]
            );
        }

        let local_addr = stream
            .local_addr()
            .trace_expect("Failed to get socket local address");

        if buffer[5] & 1 != 0 {
            if let Some(tls_config) = self.tls_config.clone() {
                info!("TLS connection received from {remote_addr}");

                return self
                    .tls_accept(stream, remote_addr, local_addr, tls_config)
                    .await;
            }
            error!("TLS requested but no TLS configuration provided");
        } else if self.session.must_use_tls {
            warn!("Peer does not support TLS, but TLS is required by configuration");
            return transport::terminate(
                codec::MessageCodec::new_framed(stream),
                codec::SessionTermReasonCode::ContactFailure,
                self.session.contact_timeout,
                &self.task_cancel_token,
            )
            .await;
        }

        info!("New TCP (NO-TLS) connection accepted from {remote_addr}");
        self.new_passive(
            local_addr,
            remote_addr,
            None,
            codec::MessageCodec::new_framed(stream),
        )
        .await
    }

    /// Handle a new passive session (server side).
    #[cfg_attr(feature = "tracing", instrument(skip(self, transport)))]
    pub async fn new_passive<T>(
        self,
        local_addr: SocketAddr,
        remote_addr: SocketAddr,
        segment_mtu: Option<usize>,
        mut transport: T,
    ) where
        T: futures::StreamExt<Item = Result<codec::Message, codec::Error>>
            + futures::SinkExt<codec::Message>
            + std::marker::Unpin,
        session::Error: From<<T as futures::Sink<codec::Message>>::Error>,
        <T as futures::Sink<codec::Message>>::Error: std::fmt::Debug,
    {
        // Read the SESS_INIT message with timeout
        let peer_init = loop {
            match transport::next_with_timeout(
                &mut transport,
                self.session.contact_timeout,
                &self.task_cancel_token,
            )
            .await
            {
                Err(e) => {
                    info!("Failed to receive SESS_INIT message: {e:?}");
                    return;
                }
                Ok(codec::Message::SessionInit(init)) => break init,
                Ok(msg) => {
                    info!("Unexpected message while waiting for SESS_INIT: {msg:?}");

                    // Send a MSG_REJECT/Unexpected message
                    if let Err(e) = transport
                        .send(codec::Message::Reject(codec::MessageRejectMessage {
                            reason_code: codec::MessageRejectionReasonCode::Unexpected,
                            rejected_message: msg.message_type() as u8,
                        }))
                        .await
                    {
                        // Its all gone wrong
                        info!("Failed to send message: {e:?}");
                        return;
                    }
                }
            };
        };

        let node_id = {
            self.node_ids
                .iter()
                .find(|node_id| {
                    matches!(
                        (&peer_init.node_id, node_id),
                        (None, _)
                            | (Some(NodeId::Ipn(_)), NodeId::Ipn(_))
                            | (Some(NodeId::Dtn(_)), NodeId::Dtn(_))
                    )
                })
                .or_else(|| self.node_ids.first())
        };

        // Send our SESS_INIT message
        if let Err(e) = transport
            .send(codec::Message::SessionInit(codec::SessionInitMessage {
                keepalive_interval: self.keepalive_interval_secs(),
                segment_mru: self.segment_mru,
                transfer_mru: self.transfer_mru,
                node_id: node_id.cloned(),
                ..Default::default()
            }))
            .await
        {
            info!("Failed to send SESS_INIT message: {e:?}");
            return;
        }

        // Negotiated KeepAlive - See RFC9174 Section 5.1.1
        let keepalive_interval = self.negotiate_keepalive(peer_init.keepalive_interval);

        // Check peer init
        for i in &peer_init.session_extensions {
            if i.flags.critical {
                // We just don't support extensions!
                return transport::terminate(
                    transport,
                    codec::SessionTermReasonCode::ContactFailure,
                    keepalive_interval * 2,
                    &self.task_cancel_token,
                )
                .await;
            }
        }

        let (tx, rx) = channel(1);
        let peer_node = peer_init.node_id.clone();
        let peer_addr = Some(hardy_bpa::cla::ClaAddress::Tcp(remote_addr));
        let cancel_token = self.session_cancel_token.clone();
        let session = session::Session::new(
            transport,
            self.sink.clone(),
            peer_node,
            peer_addr,
            Self::keepalive_as_duration(keepalive_interval),
            segment_mtu
                .map(|mtu| mtu.min(peer_init.segment_mru as usize))
                .unwrap_or(peer_init.segment_mru as usize),
            self.transfer_mru as usize,
            rx,
            cancel_token,
        );

        // Register the client for addr
        self.registry
            .register_session(
                self.sink.clone(),
                connection::Connection { tx, local_addr },
                remote_addr,
                peer_init.node_id,
            )
            .await;

        session.run().await;

        debug!("Session from {local_addr} to {remote_addr} closed");

        // Unregister the session for addr, whatever happens
        self.registry
            .unregister_session(&local_addr, &remote_addr)
            .await
    }

    /// Handle TLS accept (server side).
    #[cfg_attr(feature = "tracing", instrument(skip(self, stream)))]
    async fn tls_accept(
        self,
        stream: TcpStream,
        remote_addr: SocketAddr,
        local_addr: SocketAddr,
        tls_config: Arc<tls::TlsConfig>,
    ) {
        // This expect should be guarded by listeners not starting without TLS server config
        let acceptor = TlsAcceptor::from(
            tls_config
                .server_config
                .clone()
                .trace_expect("TLS server config not available"),
        );

        match acceptor.accept(stream).await {
            Ok(tls_stream) => {
                // TODO(mTLS): Verify client certificate if mTLS is enabled
                info!("TLS session key negotiation completed with {remote_addr}");
                self.new_passive(
                    local_addr,
                    remote_addr,
                    None,
                    codec::MessageCodec::new_framed(tls_stream),
                )
                .await;
            }
            Err(e) => {
                error!("TLS session key negotiation failed with {remote_addr}: {e}");
            }
        }
    }
}
