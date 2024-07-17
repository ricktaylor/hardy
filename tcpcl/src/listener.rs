use super::*;
use std::net::SocketAddr;
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::{Service, ServiceExt};
use utils::settings;

#[derive(Clone)]
struct Config {
    tcp_address: SocketAddr,
    contact_timeout: u16,
    use_tls: bool,
}

impl Config {
    fn new(config: &config::Config) -> Self {
        Self {
            tcp_address: settings::get_with_default::<SocketAddr, SocketAddr>(
                config,
                "tcp_address",
                "[::1]:4556".parse().unwrap(),
            )
            .trace_expect("Invalid 'tcp_address' value in configuration"),
            contact_timeout: settings::get_with_default(config, "contact_timeout", 15u16)
                .trace_expect("Invalid 'contact_timeout' value in configuration"),
            use_tls: false,
        }
    }
}

async fn new_contact(
    config: Config,
    bpa: bpa::Bpa,
    session_config: session::Config,
    mut stream: tokio::net::TcpStream,
    addr: SocketAddr,
    cancel_token: tokio_util::sync::CancellationToken,
) -> Result<(), session::Error> {
    // Receive contact header
    let mut buffer = vec![0u8; 6];
    match tokio::time::timeout(
        tokio::time::Duration::from_secs(config.contact_timeout as u64),
        stream.read_exact(&mut buffer),
    )
    .await
    {
        Ok(Ok(_)) => {
            // Parse contact header
            if buffer[0..4] != *b"dtn!" {
                return Err(session::Error::InvalidContactHeader);
            }

            info!("Contact header received from {}", addr);

            // Always send our contact header in reply!
            stream
                .write_all(&[
                    b'd',
                    b't',
                    b'n',
                    b'!',
                    4,
                    if config.use_tls { 1 } else { 0 },
                ])
                .await?;

            if buffer[4] != 4 {
                warn!("Unsupported protocol version {}", buffer[4]);

                // Terminate session
                let mut transport = codec::MessageCodec::new_framed(stream);
                return session::terminate(
                    &mut transport,
                    codec::SessionTermMessage {
                        reason_code: codec::SessionTermReasonCode::VersionMismatch,
                        ..Default::default()
                    },
                    config.contact_timeout,
                    &cancel_token,
                )
                .await;
            }

            if buffer[5] & 0xFE != 0 {
                info!(
                    "Reserved flags {:#x} set in contact header from {}",
                    buffer[5], addr,
                );
            }

            if config.use_tls && buffer[5] & 1 != 0 {
                // TLS!!
                todo!();
            } else {
                session::new_passive(
                    session_config,
                    bpa,
                    addr,
                    None,
                    codec::MessageCodec::new_framed(stream),
                    cancel_token,
                )
                .await
            }
        }
        Ok(Err(e)) => Err(e.into()),
        Err(_) => Err(session::Error::Timeout),
    }
}

struct Listener {
    listener: tokio::net::TcpListener,
    ready: Option<(tokio::net::TcpStream, SocketAddr)>,
}

impl Listener {
    fn new(listener: tokio::net::TcpListener) -> Self {
        Self {
            listener,
            ready: None,
        }
    }
}

impl tower::Service<()> for Listener {
    type Response = (tokio::net::TcpStream, SocketAddr);
    type Error = std::io::Error;
    type Future =
        Pin<Box<dyn futures::Future<Output = Result<Self::Response, Self::Error>> + Send>>;

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

#[instrument(skip_all)]
async fn accept(
    config: Config,
    bpa: bpa::Bpa,
    session_config: session::Config,
    cancel_token: tokio_util::sync::CancellationToken,
) {
    let listener = Listener::new(
        tokio::net::TcpListener::bind(config.tcp_address)
            .await
            .trace_expect("Failed to bind TCP listener"),
    );

    info!("TCP server listening on {}", config.tcp_address);

    // TODO: We can layer services here
    let mut svc = tower::ServiceBuilder::new()
        //.rate_limit(1024, std::time::Duration::from_secs(1))
        .service(listener);

    let mut task_set = tokio::task::JoinSet::new();

    loop {
        tokio::select! {
            // Wait for the service to be ready
            r = svc.ready() => match r {
                Ok(_) => {
                    // Accept a new connection
                    match svc.call(()).await {
                        Ok((stream,addr)) => {
                            // Spawn immediately to prevent head-of-line blocking
                            let cancel_token_cloned = cancel_token.clone();
                            let config_cloned = config.clone();
                            let bpa_cloned = bpa.clone();
                            let session_config_cloned = session_config.clone();

                            task_set.spawn(async move {
                                if let Err(e) = new_contact(config_cloned, bpa_cloned, session_config_cloned, stream, addr, cancel_token_cloned).await {
                                    warn!("Contact failed: {e}");
                                }
                            });
                        }
                        Err(e) => warn!("Failed to accept connection: {e}")
                    }
                }
                Err(e) => {
                    warn!("Listener closed: {e}");
                    break;
                }
            },
            Some(r) = task_set.join_next() => r.trace_expect("Task terminated unexpectedly"),
            _ = cancel_token.cancelled() => break
        }
    }

    // Wait for all sub-tasks to complete
    while let Some(r) = task_set.join_next().await {
        r.trace_expect("Task terminated unexpectedly");
    }
}

#[instrument(skip_all)]
pub fn init(
    config: &config::Config,
    bpa: bpa::Bpa,
    task_set: &mut tokio::task::JoinSet<()>,
    cancel_token: tokio_util::sync::CancellationToken,
) {
    let session_config = session::Config::new(config);

    let config = Config::new(config);
    if config.contact_timeout > 60 {
        warn!("RFC9174 specifies contact timeout SHOULD be a maximum of 60 seconds");
    }

    // Start listening
    task_set.spawn(accept(config, bpa, session_config, cancel_token));
}
