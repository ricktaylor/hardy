use super::*;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc::*,
};

pub struct Connector {
    pub cancel_token: tokio_util::sync::CancellationToken,
    pub task_tracker: tokio_util::task::TaskTracker,
    pub contact_timeout: u16,
    pub use_tls: bool,
    pub keepalive_interval: Option<u16>,
    pub segment_mru: u64,
    pub transfer_mru: u64,
    pub node_ids: Arc<[bpv7::Eid]>,
    pub sink: Arc<dyn hardy_bpa::cla::Sink>,
    pub registry: Arc<connection::ConnectionRegistry>,
}

impl Connector {
    pub async fn connect(
        self: Connector,
        remote_addr: &SocketAddr,
    ) -> Result<(), transport::Error> {
        let mut stream = TcpStream::connect(remote_addr)
            .await
            .inspect_err(|e| trace!("Failed to TCP connect to {remote_addr}: {e}"))?;

        // Send contact header
        stream
            .write_all(&[b'd', b't', b'n', b'!', 4, if self.use_tls { 1 } else { 0 }])
            .await
            .inspect_err(|e| trace!("Failed to send contact header: {e}"))?;

        // Receive contact header
        let mut buffer = [0u8; 6];
        tokio::time::timeout(
            tokio::time::Duration::from_secs(self.contact_timeout as u64),
            stream.read_exact(&mut buffer),
        )
        .await
        .map_err(|_| transport::Error::Timeout)
        .inspect_err(|_| trace!("Connection timed out"))?
        .inspect_err(|e| trace!("Read failed: {e}"))?;

        // Parse contact header
        if buffer[0..4] != *b"dtn!" {
            trace!("Contact header isn't: 'dtn!'");
            return Err(transport::Error::InvalidProtocol);
        }

        trace!("Contact header received from {}", remote_addr);

        if buffer[4] != 4 {
            warn!("Unsupported protocol version {}", buffer[4]);

            // Terminate session
            transport::terminate(
                codec::MessageCodec::new_framed(stream),
                codec::SessionTermReasonCode::VersionMismatch,
                self.contact_timeout,
                &self.cancel_token,
            )
            .await;
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

        if self.use_tls && buffer[5] & 1 != 0 {
            // TLS!!
            todo!();
        } else {
            self.new_active(
                local_addr,
                remote_addr,
                None,
                codec::MessageCodec::new_framed(stream),
            )
            .await
        }
    }

    async fn new_active<T>(
        self: Connector,
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
        self.task_tracker.spawn(async move {
            // Register the client for addr
            registry
                .register_session(
                    connection::Connection { tx, local_addr },
                    remote_addr,
                    peer_init.node_id,
                )
                .await;

            session.run().await;

            trace!("Session with {remote_addr} closed");

            // Unregister the session for addr, whatever happens
            registry.unregister_session(&local_addr, &remote_addr).await
        });
        Ok(())
    }
}
