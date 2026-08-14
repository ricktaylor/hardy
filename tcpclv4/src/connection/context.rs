use core::num::NonZeroU64;
use std::net::SocketAddr;

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc::*,
};

use super::*;

/// Data needed to handle a connection, without the TaskPool to avoid circular references.
///
/// This struct is shared between active (Connector) and passive (Listener) connection handling.
/// It contains all the configuration and state needed to negotiate and run a TCPCLv4 session,
/// but excludes the TaskPool to prevent Arc cycles when spawning tasks.
#[derive(Clone)]
pub struct ConnectionContext {
    pub contact_timeout: ContactTimeout,
    pub keepalive_interval: KeepaliveInterval,
    pub segment_mru: NonZeroU64,
    pub transfer_mru: NonZeroU64,
    pub node_ids: Arc<[NodeId]>,
    pub sink: Arc<dyn hardy_bpa::cla::Sink>,
    pub registry: Arc<connection::ConnectionRegistry>,
    pub tls: Option<Arc<tls::Tls>>,
    pub session_cancel_token: tokio_util::sync::CancellationToken,
    pub task_cancel_token: hardy_async::CancellationToken,
}

impl std::fmt::Debug for ConnectionContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionContext")
            .field("contact_timeout", &self.contact_timeout)
            .field("keepalive_interval", &self.keepalive_interval)
            .field("segment_mru", &self.segment_mru)
            .field("transfer_mru", &self.transfer_mru)
            .field("node_ids", &self.node_ids)
            .field("tls", &self.tls)
            .finish_non_exhaustive()
    }
}

impl ConnectionContext {
    /// Returns the contact timeout as a Duration.
    pub fn contact_timeout_duration(&self) -> tokio::time::Duration {
        tokio::time::Duration::from_secs(u64::from(self.contact_timeout.get()))
    }

    // The CAN_TLS flag must be honest per role (RFC 9174 Section 4.2:
    // TLS is used only when both peers set it): the dialing side can
    // always play the TLS client when material is configured, but the
    // accepting side can only serve TLS with an identity.
    fn contact_header(can_tls: bool) -> [u8; 6] {
        [b'd', b't', b'n', b'!', 4, u8::from(can_tls)]
    }

    /// The 6-byte contact header sent when dialing (RFC 9174 Section 4.2).
    pub fn dialing_contact_header(&self) -> [u8; 6] {
        Self::contact_header(self.tls.is_some())
    }

    /// The 6-byte contact header sent when accepting (RFC 9174 Section 4.2).
    pub fn accepting_contact_header(&self) -> [u8; 6] {
        Self::contact_header(self.tls.as_ref().is_some_and(|tls| tls.has_identity()))
    }

    /// Get the first configured node ID.
    pub fn first_node_id(&self) -> Option<NodeId> {
        self.node_ids.first().cloned()
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
    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    pub async fn new_contact(self, mut stream: TcpStream, remote_addr: SocketAddr) {
        // Disable Nagle's algorithm to ensure timely delivery of small messages
        // like XFER_ACK, KEEPALIVE, and SESS_TERM
        stream
            .set_nodelay(true)
            .inspect_err(|e| debug!("Failed to set TCP_NODELAY: {e}"))
            .ok();

        let local_addr = stream
            .local_addr()
            .trace_expect("Failed to get socket local address");

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
                debug!(%local_addr, %remote_addr, "Read failed: {e}");
                return;
            }
            Err(_) => {
                debug!(%local_addr, %remote_addr, "Connection timed out");
                return;
            }
        }

        // Parse contact header
        if buffer[0..4] != *b"dtn!" {
            debug!(%local_addr, %remote_addr, "Contact header isn't: 'dtn!'");
            return;
        }

        debug!(%local_addr, %remote_addr, "Contact header received");

        // Always send our contact header in reply!
        if let Err(e) = stream.write_all(&self.accepting_contact_header()).await {
            debug!(%local_addr, %remote_addr, "Failed to send contact header: {e}");
            return;
        }

        if buffer[4] != 4 {
            debug!(%local_addr, %remote_addr, "Unsupported protocol version {}", buffer[4]);

            // Terminate session
            return transport::terminate(
                codec::MessageCodec::new_framed(stream),
                codec::SessionTermReasonCode::VersionMismatch,
                self.contact_timeout.get(),
                &self.task_cancel_token,
            )
            .await;
        }

        if buffer[5] & 0xFE != 0 {
            debug!(%local_addr, %remote_addr, "Reserved flags {:#x} set in contact header", buffer[5]);
        }

        if buffer[5] & 1 != 0 {
            if let Some(tls_config) = self.tls.clone()
                && tls_config.has_identity()
            {
                debug!(%local_addr, %remote_addr, "TLS connection received");
                return self
                    .tls_accept(stream, remote_addr, local_addr, tls_config)
                    .await;
            }
            // Our accepting header did not advertise TLS, so a peer flag
            // here does not commit the session to it (RFC 9174 Section
            // 4.2: TLS is used only when both peers set the flag)
            debug!(%local_addr, %remote_addr, "TLS requested by peer, but this listener cannot serve TLS (no identity configured)");
        } else if self.tls.as_ref().is_some_and(|tls| tls.is_required()) {
            warn!(%local_addr, %remote_addr, "Peer does not support TLS, but TLS is required by configuration");
            return transport::terminate(
                codec::MessageCodec::new_framed(stream),
                codec::SessionTermReasonCode::ContactFailure,
                self.contact_timeout.get(),
                &self.task_cancel_token,
            )
            .await;
        }

        debug!(%local_addr, %remote_addr, "New TCP (NO-TLS) connection accepted");
        self.new_passive(
            local_addr,
            remote_addr,
            None,
            codec::MessageCodec::new_framed(stream),
        )
        .await
    }

    /// Handle a new passive session (server side).
    #[cfg_attr(feature = "instrument", instrument(skip(self, transport)))]
    pub async fn new_passive<T>(
        self,
        local_addr: SocketAddr,
        remote_addr: SocketAddr,
        segment_mtu: Option<usize>,
        mut transport: T,
    ) where
        T: futures::StreamExt<Item = Result<codec::Message, codec::Error>>
            + futures::SinkExt<codec::Message, Error = codec::Error>
            + std::marker::Unpin
            + Send
            + 'static,
    {
        // Read the SESS_INIT message with timeout
        let peer_init = loop {
            match transport::next_with_timeout(
                &mut transport,
                self.contact_timeout.get(),
                &self.task_cancel_token,
            )
            .await
            {
                Err(e) => {
                    debug!(%local_addr, %remote_addr, "Failed to receive SESS_INIT message: {e:?}");
                    return;
                }
                Ok(codec::Message::SessionInit(init)) => break init,
                Ok(msg) => {
                    debug!(%local_addr, %remote_addr, "Unexpected message while waiting for SESS_INIT: {msg:?}");

                    // Send a MSG_REJECT/Unexpected message
                    if let Err(e) = transport
                        .send(codec::Message::Reject(codec::MessageRejectMessage {
                            reason_code: codec::MessageRejectionReasonCode::Unexpected,
                            rejected_message: msg.message_type() as u8,
                        }))
                        .await
                    {
                        // Its all gone wrong
                        debug!(%local_addr, %remote_addr, "Failed to send message: {e:?}");
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
                keepalive_interval: self.keepalive_interval.get(),
                segment_mru: self.segment_mru.get(),
                transfer_mru: self.transfer_mru.get(),
                node_id: node_id.cloned(),
                ..Default::default()
            }))
            .await
        {
            debug!(%local_addr, %remote_addr, "Failed to send SESS_INIT message: {e:?}");
            return;
        }

        // Negotiated KeepAlive - See RFC9174 Section 5.1.1
        let keepalive_interval = self
            .keepalive_interval
            .negotiate(peer_init.keepalive_interval)
            .get();

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
        let keepalive_duration = Self::keepalive_as_duration(keepalive_interval);

        // Split the transport into reader and writer halves
        // This allows the writer task to send keepalives independently of the
        // session loop, preventing session timeout when dispatch() blocks.
        let (transport_writer, transport_reader) = transport.split();

        // Create the writer task (handles keepalives independently)
        let (writer_handle, writer_task) =
            writer::create_writer(transport_writer, keepalive_duration, cancel_token.clone());

        // Spawn the writer task (not in a TaskPool - the session owns
        // the writer's lifecycle via WriteCommand::Close and cancel_token)
        let task = async move {
            writer_task.run().await;
        };
        #[cfg(feature = "instrument")]
        let task = {
            let span = tracing::trace_span!(parent: None, "passive_session_writer");
            span.follows_from(tracing::Span::current());
            tracing::Instrument::instrument(task, span)
        };
        tokio::spawn(task);

        let (session, ingest_rx) = session::Session::new(
            transport_reader,
            writer_handle,
            self.sink.clone(),
            peer_node,
            peer_addr,
            keepalive_duration,
            negotiate_segment_mtu(segment_mtu, peer_init.segment_mru),
            usize::try_from(self.transfer_mru.get()).unwrap_or(usize::MAX),
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

        metrics::counter!("tcpclv4.session.established").increment(1);

        session.run(ingest_rx).await;

        debug!(%local_addr, %remote_addr, "Session closed");

        // Unregister the session for addr, whatever happens
        self.registry
            .unregister_session(&local_addr, &remote_addr)
            .await
    }

    /// Handle TLS accept (server side).
    #[cfg_attr(feature = "instrument", instrument(skip(self, stream)))]
    async fn tls_accept(
        self,
        stream: TcpStream,
        remote_addr: SocketAddr,
        local_addr: SocketAddr,
        tls_config: Arc<tls::Tls>,
    ) {
        // Guarded by the has_identity() check before tls_accept is called
        let acceptor = tls_config
            .acceptor()
            .trace_expect("TLS server config not available");

        match acceptor.accept(stream).await {
            Ok(tls_stream) => {
                // Client certificates are requested and verified inside the
                // handshake, per the configured client-auth policy, so a
                // verified peer holds a certificate from a trusted CA.
                // TODO(RFC 9174 Section 4.4.4.3): bind the certificate to
                // the peer's Node ID (match the SESS_INIT NODE-ID against
                // the certificate), so a verified peer is the node it
                // claims to be, not merely a member of the PKI.
                debug!(%local_addr, %remote_addr, "TLS session key negotiation completed");
                self.new_passive(
                    local_addr,
                    remote_addr,
                    None,
                    codec::MessageCodec::new_framed(tls_stream),
                )
                .await;
            }
            Err(e) => {
                debug!(%local_addr, %remote_addr, "TLS session key negotiation failed: {e}");
            }
        }
    }
}

// Negotiate the outbound segment MTU: our configured MTU capped by the
// peer's advertised segment MRU. The wire-derived u64 is clamped, not
// truncated: on a 32-bit target `as usize` can truncate a large MRU to 0,
// turning the sender's segmentation loop into an infinite empty-segment
// spin.
pub fn negotiate_segment_mtu(local_mtu: Option<usize>, peer_segment_mru: u64) -> usize {
    let peer_segment_mru = usize::try_from(peer_segment_mru).unwrap_or(usize::MAX);
    local_mtu
        .map(|mtu| mtu.min(peer_segment_mru))
        .unwrap_or(peer_segment_mru)
}

#[cfg(test)]
mod tests {
    use super::*;

    // UT-TCP-03: segment MTU negotiation, including the 32-bit clamp.
    #[test]
    fn negotiate_segment_mtu_cases() {
        assert_eq!(negotiate_segment_mtu(Some(8192), 16384), 8192);
        assert_eq!(negotiate_segment_mtu(None, 16384), 16384);
        assert_eq!(negotiate_segment_mtu(None, u64::MAX), usize::MAX);
        assert_eq!(negotiate_segment_mtu(Some(8192), u64::MAX), 8192);
    }
}
