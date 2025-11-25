use super::*;
use std::{
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc::*,
};
use tower::{Service, ServiceExt};

struct ListenerService {
    listener: TcpListener,
    ready: Option<(TcpStream, SocketAddr)>,
}

impl ListenerService {
    fn new(listener: TcpListener) -> Self {
        Self {
            listener,
            ready: None,
        }
    }
}

impl tower::Service<()> for ListenerService {
    type Response = (TcpStream, SocketAddr);
    type Error = std::io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.listener.poll_accept(cx).map_ok(|(s, a)| {
            self.ready = Some((s, a));
        })
    }

    fn call(&mut self, _: ()) -> Self::Future {
        let (s, a) = self.ready.take().trace_expect("poll_ready not called");
        Box::pin(async move { Ok((s, a)) })
    }
}

pub struct Listener {
    pub cancel_token: tokio_util::sync::CancellationToken,
    pub task_tracker: tokio_util::task::TaskTracker,
    pub contact_timeout: u16,
    pub use_tls: bool,
    pub keepalive_interval: Option<u16>,
    pub segment_mru: u64,
    pub transfer_mru: u64,
    pub node_ids: Arc<[Eid]>,
    pub sink: Arc<dyn hardy_bpa::cla::Sink>,
    pub registry: Arc<connection::ConnectionRegistry>,
}

impl std::fmt::Debug for Listener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listener")
            //.field("cancel_token", &self.cancel_token)
            .field("contact_timeout", &self.contact_timeout)
            .field("use_tls", &self.use_tls)
            .field("keepalive_interval", &self.keepalive_interval)
            .field("segment_mru", &self.segment_mru)
            .field("transfer_mru", &self.transfer_mru)
            .field("node_ids", &self.node_ids)
            //.field("sink", &self.sink)
            //.field("registry", &self.registry)
            .finish()
    }
}

impl Listener {
    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    pub async fn listen(self: Arc<Listener>, address: std::net::SocketAddr) {
        let Ok(listener) = TcpListener::bind(address)
            .await
            .inspect_err(|e| error!("Failed to bind TCP listener: {e:?}"))
        else {
            return;
        };

        // We can layer services here
        let mut svc = tower::ServiceBuilder::new()
            .rate_limit(1024, std::time::Duration::from_secs(1))
            .service(ListenerService::new(listener));

        info!("TCP server listening on {}", address);

        loop {
            tokio::select! {
                // Wait for the service to be ready
                r = svc.ready() => match r {
                    Ok(_) => {
                        // Accept a new connection
                        match svc.call(()).await {
                            Ok((stream,remote_addr)) => {
                                // Spawn immediately to prevent head-of-line blocking
                                let self_cloned = self.clone();
                                self.task_tracker.spawn(self_cloned.new_contact(stream, remote_addr));
                            }
                            Err(e) => warn!("Failed to accept connection: {e}")
                        }
                    }
                    Err(e) => {
                        warn!("Listener closed: {e}");
                        break;
                    }
                },
                _ = self.cancel_token.cancelled() => break
            }
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    async fn new_contact(self: Arc<Listener>, mut stream: TcpStream, remote_addr: SocketAddr) {
        // Receive contact header
        let mut buffer = [0u8; 6];
        match tokio::time::timeout(
            tokio::time::Duration::from_secs(self.contact_timeout as u64),
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

        debug!("Contact header received from {}", remote_addr);

        // Always send our contact header in reply!
        if let Err(e) = stream
            .write_all(&[b'd', b't', b'n', b'!', 4, if self.use_tls { 1 } else { 0 }])
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
                self.contact_timeout,
                &self.cancel_token,
            )
            .await;
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
            self.new_passive(
                local_addr,
                remote_addr,
                None,
                codec::MessageCodec::new_framed(stream),
            )
            .await;
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self, transport)))]
    async fn new_passive<T>(
        self: Arc<Listener>,
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
                self.contact_timeout,
                &self.cancel_token,
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
                .find(|eid| {
                    matches!(
                        (&peer_init.node_id, eid),
                        (None, _)
                            | (Some(Eid::Ipn { .. }), Eid::Ipn { .. })
                            | (Some(Eid::Dtn { .. }), Eid::Dtn { .. })
                    )
                })
                .or_else(|| self.node_ids.first())
        };

        // Send our SESS_INIT message
        if let Err(e) = transport
            .send(codec::Message::SessionInit(codec::SessionInitMessage {
                keepalive_interval: self.keepalive_interval.unwrap_or(0),
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
        let keepalive_interval = self
            .keepalive_interval
            .map(|keepalive_interval| peer_init.keepalive_interval.min(keepalive_interval))
            .unwrap_or(0);

        // Check peer init
        for i in &peer_init.session_extensions {
            if i.flags.critical {
                // We just don't support extensions!
                return transport::terminate(
                    transport,
                    codec::SessionTermReasonCode::ContactFailure,
                    keepalive_interval * 2,
                    &self.cancel_token,
                )
                .await;
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

        // Register the client for addr
        if self
            .registry
            .register_session(
                connection::Connection { tx, local_addr },
                remote_addr,
                peer_init.node_id,
            )
            .await
        {
            session.run().await;
        }

        debug!("Session with {remote_addr} closed");

        // Unregister the session for addr, whatever happens
        self.registry
            .unregister_session(&local_addr, &remote_addr)
            .await
    }
}
