use super::*;
use std::net::SocketAddr;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc::*,
};
use tokio_rustls::TlsConnector;

pub struct Connector {
    pub tasks: Arc<hardy_async::TaskPool>,
    pub ctx: context::ConnectionContext,
}

impl std::fmt::Debug for Connector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connector")
            .field("ctx", &self.ctx)
            .finish_non_exhaustive()
    }
}

impl Connector {
    #[cfg_attr(feature = "tracing", instrument)]
    pub async fn connect(self, remote_addr: &SocketAddr) -> Result<(), transport::Error> {
        let mut stream = TcpStream::connect(remote_addr)
            .await
            .inspect_err(|e| debug!("Failed to TCP connect to {remote_addr}: {e}"))?;

        // Send contact header
        stream
            .write_all(&[b'd', b't', b'n', b'!', 4, self.ctx.tls_contact_flag()])
            .await
            .inspect_err(|e| debug!("Failed to send contact header: {e}"))?;

        // Receive contact header
        let mut buffer = [0u8; 6];
        tokio::time::timeout(
            self.ctx.contact_timeout_duration(),
            stream.read_exact(&mut buffer),
        )
        .await
        .map_err(|_| transport::Error::Timeout)
        .inspect_err(|_| debug!("Connection timed out"))?
        .inspect_err(|e| debug!("Read failed: {e}"))?;

        // Parse contact header
        if buffer[0..4] != *b"dtn!" {
            debug!("Contact header isn't: 'dtn!'");
            return Err(transport::Error::InvalidProtocol);
        }

        debug!("Contact header received from {remote_addr}");

        if buffer[4] != 4 {
            warn!("Unsupported protocol version {}", buffer[4]);

            if buffer[4] == 3 {
                debug!("Sending TCPCLv3 SHUTDOWN message to {remote_addr}");

                // Send a TCPCLv3 SHUTDOWN message
                stream
                    .write_all(&[0x45, 0x01])
                    .await
                    .inspect_err(|e| debug!("Failed to send TCPv3 SHUTDOWN message: {e}"))?;
                stream.shutdown().await?;
            } else {
                // Terminate session
                transport::terminate(
                    codec::MessageCodec::new_framed(stream),
                    codec::SessionTermReasonCode::VersionMismatch,
                    self.ctx.session.contact_timeout,
                    &self.ctx.task_cancel_token,
                )
                .await;
            }
            return Err(transport::Error::InvalidProtocol);
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
            if let Some(tls_config) = self.ctx.tls_config.clone() {
                info!("Initiating TLS handshake with {remote_addr}");
                return self
                    .tls_handshake(stream, remote_addr, local_addr, tls_config)
                    .await
                    .inspect_err(|e| {
                        error!("TLS session negotiation failed to {remote_addr}: {e}")
                    });
            }
            info!("TLS requested by peer but no TLS configuration provided");
        } else if self.ctx.session.must_use_tls {
            warn!("Peer does not support TLS, but TLS is required by configuration");
            transport::terminate(
                codec::MessageCodec::new_framed(stream),
                codec::SessionTermReasonCode::ContactFailure,
                self.ctx.session.contact_timeout,
                &self.ctx.task_cancel_token,
            )
            .await;

            return Err(transport::Error::InvalidProtocol);
        }

        info!("New TCP (NO-TLS) connection connected to {remote_addr}");
        self.new_active(
            local_addr,
            remote_addr,
            None,
            codec::MessageCodec::new_framed(stream),
        )
        .await
    }

    #[cfg_attr(feature = "tracing", instrument(skip(stream)))]
    async fn tls_handshake(
        self: Connector,
        stream: TcpStream,
        remote_addr: &SocketAddr,
        local_addr: SocketAddr,
        tls_config: Arc<tls::TlsConfig>,
    ) -> Result<(), transport::Error> {
        // Priority: configured name > localhost (loopback) > IP address
        let server_name = if let Some(configured_name) = &tls_config.server_name {
            // Use the configured server name (for certificates issued to domain names)
            rustls::pki_types::ServerName::try_from(configured_name.clone()).map_err(|e| {
                error!("Invalid configured server name for TLS: {e}");
                transport::Error::InvalidProtocol
            })?
        } else if remote_addr.ip().is_loopback() {
            // Fallback: localhost for loopback connections
            rustls::pki_types::ServerName::try_from("localhost").map_err(|e| {
                error!("Invalid server name for TLS: {e}");
                transport::Error::InvalidProtocol
            })?
        } else {
            // Fallback: IP address (may fail if certificate is for a domain name)
            rustls::pki_types::ServerName::from(remote_addr.ip())
        };

        // Use tokio-rustls::TlsConnector - simple wrapper around rustls for async I/O
        let connector = TlsConnector::from(tls_config.client_config.clone());
        let tls_stream = connector.connect(server_name, stream).await.map_err(|e| {
            error!("TLS session key negotiation failed to {remote_addr}: {e}");
            transport::Error::InvalidProtocol
        })?;

        // TODO(mTLS): Verify that server accepted our client certificate if mTLS is enabled
        info!("TLS session key negotiation completed to {remote_addr}");

        self.new_active(
            local_addr,
            remote_addr,
            None,
            codec::MessageCodec::new_framed(tls_stream),
        )
        .await
    }

    #[cfg_attr(feature = "tracing", instrument(skip(transport)))]
    async fn new_active<T>(
        self,
        local_addr: SocketAddr,
        remote_addr: &SocketAddr,
        segment_mtu: Option<usize>,
        mut transport: T,
    ) -> Result<(), transport::Error>
    where
        T: futures::StreamExt<Item = Result<codec::Message, codec::Error>>
            + futures::SinkExt<codec::Message>
            + std::marker::Unpin
            + Send
            + 'static,
        session::Error: From<<T as futures::Sink<codec::Message>>::Error>,
        <T as futures::Sink<codec::Message>>::Error: Into<transport::Error> + std::fmt::Debug,
    {
        debug!("Sending SESS_INIT to {remote_addr}");

        // Send our SESS_INIT message
        transport
            .send(codec::Message::SessionInit(codec::SessionInitMessage {
                keepalive_interval: self.ctx.keepalive_interval_secs(),
                segment_mru: self.ctx.segment_mru,
                transfer_mru: self.ctx.transfer_mru,
                node_id: self.ctx.first_node_id(),
                ..Default::default()
            }))
            .await
            .inspect_err(|e| info!("Failed to send SESS_INIT message: {e:?}"))
            .map_err(Into::into)?;

        debug!("Reading SESS_INIT from {remote_addr}");

        // Read the SESS_INIT message with timeout
        let peer_init = loop {
            match transport::next_with_timeout(
                &mut transport,
                self.ctx.session.contact_timeout,
                &self.ctx.task_cancel_token,
            )
            .await
            .inspect_err(|e| info!("Failed to receive SESS_INIT message: {e:?}"))?
            {
                codec::Message::SessionInit(init) => break init,
                msg => {
                    info!("Unexpected message while waiting for SESS_INIT: {msg:?}");

                    // Send a MSG_REJECT/Unexpected message
                    transport
                        .send(codec::Message::Reject(codec::MessageRejectMessage {
                            reason_code: codec::MessageRejectionReasonCode::Unexpected,
                            rejected_message: msg.message_type() as u8,
                        }))
                        .await
                        .inspect_err(|e| info!("Failed to send message: {e:?}"))
                        .map_err(Into::into)?;
                }
            };
        };

        debug!("Received SESS_INIT {peer_init:?} from {remote_addr}");

        // Negotiated KeepAlive - See RFC9174 Section 5.1.1
        let keepalive_interval = self.ctx.negotiate_keepalive(peer_init.keepalive_interval);

        // Check peer init
        for i in &peer_init.session_extensions {
            if i.flags.critical {
                // We just don't support extensions!
                transport::terminate(
                    transport,
                    codec::SessionTermReasonCode::ContactFailure,
                    keepalive_interval * 2,
                    &self.ctx.task_cancel_token,
                )
                .await;
                return Err(transport::Error::InvalidProtocol);
            }
        }

        let (tx, rx) = channel(1);
        let peer_node = peer_init.node_id.clone();
        let peer_addr = Some(hardy_bpa::cla::ClaAddress::Tcp(*remote_addr));
        let cancel_token = self.ctx.session_cancel_token.clone();
        let session = session::Session::new(
            transport,
            self.ctx.sink.clone(),
            peer_node,
            peer_addr,
            context::ConnectionContext::keepalive_as_duration(keepalive_interval),
            segment_mtu
                .map(|mtu| mtu.min(peer_init.segment_mru as usize))
                .unwrap_or(peer_init.segment_mru as usize),
            self.ctx.transfer_mru as usize,
            rx,
            cancel_token,
        );

        // Kick off the run() as a background task
        // Extract what we need to avoid capturing `self` (which has Arc<TaskPool>)
        let registry = self.ctx.registry.clone();
        let sink = self.ctx.sink.clone();
        let remote_addr = *remote_addr;

        hardy_async::spawn!(self.tasks, "active_session_task", async move {
            registry
                .register_session(
                    sink,
                    connection::Connection { tx, local_addr },
                    remote_addr,
                    peer_init.node_id,
                )
                .await;

            session.run().await;

            debug!("Session from {local_addr} to {remote_addr} closed");

            // Unregister the session for addr, whatever happens
            registry.unregister_session(&local_addr, &remote_addr).await
        });

        Ok(())
    }
}
