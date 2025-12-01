use super::*;
use std::net::SocketAddr;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc::*,
};
use tokio_rustls::TlsConnector;

pub struct Connector {
    pub cancel_token: tokio_util::sync::CancellationToken,
    pub task_tracker: tokio_util::task::TaskTracker,
    pub contact_timeout: u16,
    pub must_use_tls: bool,
    pub keepalive_interval: Option<u16>,
    pub segment_mru: u64,
    pub transfer_mru: u64,
    pub node_ids: Arc<[NodeId]>,
    pub sink: Arc<dyn hardy_bpa::cla::Sink>,
    pub registry: Arc<connection::ConnectionRegistry>,
    pub tls_config: Option<Arc<tls::TlsConfig>>,
}

impl std::fmt::Debug for Connector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connector")
            //.field("cancel_token", &self.cancel_token)
            //.field("task_tracker", &self.task_tracker)
            .field("contact_timeout", &self.contact_timeout)
            .field("must_use_tls", &self.must_use_tls)
            .field("keepalive_interval", &self.keepalive_interval)
            .field("segment_mru", &self.segment_mru)
            .field("transfer_mru", &self.transfer_mru)
            .field("node_ids", &self.node_ids)
            //.field("sink", &self.sink)
            //.field("registry", &self.registry)
            .field("tls_config", &self.tls_config)
            .finish()
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
            .write_all(&[
                b'd',
                b't',
                b'n',
                b'!',
                4,
                if self.tls_config.is_some() { 1 } else { 0 },
            ])
            .await
            .inspect_err(|e| debug!("Failed to send contact header: {e}"))?;

        // Receive contact header
        let mut buffer = [0u8; 6];
        tokio::time::timeout(
            tokio::time::Duration::from_secs(self.contact_timeout as u64),
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

        debug!("Contact header received from {}", remote_addr);

        if buffer[4] != 4 {
            warn!("Unsupported protocol version {}", buffer[4]);

            if buffer[4] == 3 {
                debug!("Sending TCPCLv3 SHUTDOWN message to {}", remote_addr);

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
                    self.contact_timeout,
                    &self.cancel_token,
                )
                .await;
            }
            return Err(transport::Error::InvalidProtocol);
        }

        if buffer[5] & 0xFE != 0 {
            info!(
                "Reserved flags {:#x} set in contact header from {}",
                buffer[5], remote_addr,
            );
        }

        let local_addr = stream
            .local_addr()
            .trace_expect("Failed to get socket local address");

        if buffer[5] & 1 != 0 {
            if let Some(tls_config) = self.tls_config.clone() {
                info!("Initiating TLS handshake with {}", remote_addr);
                return self
                    .tls_handshake(stream, remote_addr, local_addr, tls_config)
                    .await
                    .inspect_err(|e| {
                        error!("TLS session negotiation failed to {}: {e}", remote_addr)
                    });
            }
            info!("TLS requested by peer but no TLS configuration provided");
        } else if self.must_use_tls {
            warn!("Peer does not support TLS, but TLS is required by configuration");
            transport::terminate(
                codec::MessageCodec::new_framed(stream),
                codec::SessionTermReasonCode::ContactFailure,
                self.contact_timeout,
                &self.cancel_token,
            )
            .await;

            return Err(transport::Error::InvalidProtocol);
        }

        info!("New TCP (NO-TLS) connection connected to {}", remote_addr);
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
            error!("TLS session key negotiation failed to {}: {e}", remote_addr);
            transport::Error::InvalidProtocol
        })?;

        // TODO(mTLS): Verify that server accepted our client certificate if mTLS is enabled
        info!("TLS session key negotiation completed to {}", remote_addr);

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
                keepalive_interval: self.keepalive_interval.unwrap_or(0),
                segment_mru: self.segment_mru,
                transfer_mru: self.transfer_mru,
                node_id: self.node_ids.first().cloned(),
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
                self.contact_timeout,
                &self.cancel_token,
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
        let keepalive_interval = self
            .keepalive_interval
            .map(|keepalive_interval| peer_init.keepalive_interval.min(keepalive_interval))
            .unwrap_or(0);

        // Check peer init
        for i in &peer_init.session_extensions {
            if i.flags.critical {
                // We just don't support extensions!
                transport::terminate(
                    transport,
                    codec::SessionTermReasonCode::ContactFailure,
                    keepalive_interval * 2,
                    &self.cancel_token,
                )
                .await;
                return Err(transport::Error::InvalidProtocol);
            }
        }

        let (tx, rx) = channel(1);
        let session = session::Session::new(
            transport,
            self.sink.clone(),
            if keepalive_interval != 0 {
                Some(tokio::time::Duration::from_secs(keepalive_interval as u64))
            } else {
                None
            },
            segment_mtu
                .map(|mtu| mtu.min(peer_init.segment_mru as usize))
                .unwrap_or(peer_init.segment_mru as usize),
            self.transfer_mru as usize,
            rx,
        );

        // Kick off the run() as a background task
        let registry = self.registry.clone();
        let remote_addr = *remote_addr;

        let task = async move {
            registry
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
            registry.unregister_session(&local_addr, &remote_addr).await
        };

        #[cfg(feature = "tracing")]
        let task = {
            let span = tracing::trace_span!(
                parent: None,
                "active_session_task"
            );
            span.follows_from(tracing::Span::current());
            task.instrument(span)
        };

        self.task_tracker.spawn(task);
        Ok(())
    }
}
