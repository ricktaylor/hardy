use super::*;
use futures::sink::SinkExt;
use futures::stream::StreamExt;
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use utils::settings;

#[derive(Clone)]
struct Config {
    tcp_address: SocketAddr,
    contact_timeout: u64,
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
            contact_timeout: settings::get_with_default(config, "contact_timeout", 15u64)
                .trace_expect("Invalid 'contact_timeout' value in configuration"),
            use_tls: false,
        }
    }
}

#[instrument(skip(config, cancel_token))]
async fn new_contact(
    config: Config,
    mut stream: tokio::net::TcpStream,
    addr: SocketAddr,
    cancel_token: tokio_util::sync::CancellationToken,
) {
    // Receive contact header
    let mut buffer = vec![0u8; 6];
    match tokio::time::timeout(
        tokio::time::Duration::from_secs(config.contact_timeout),
        stream.read_exact(&mut buffer),
    )
    .await
    {
        Ok(Ok(_)) => {
            // Parse contact header
            if buffer[0..4] != *b"dtn!" {
                return warn!("Invalid contact header from {addr}");
            }

            info!("Contact header received from {addr}");

            // Always send our contact header in reply!
            if let Err(e) = stream
                .write_all(&[
                    b'd',
                    b't',
                    b'n',
                    b'!',
                    4,
                    if config.use_tls { 1 } else { 0 },
                ])
                .await
            {
                return error!("Failed to send contact header: {e}");
            }

            if buffer[4] != 4 {
                warn!("Unsupported protocol version {}", buffer[4]);

                // Terminate session
                let mut transport = codec::MessageCodec::new_framed(stream);
                if let Err(e) = session::send_session_term(
                    &mut transport,
                    codec::SessionTermMessage {
                        reason_code: codec::SessionTermReasonCode::VersionMismatch,
                        ..Default::default()
                    },
                    config.contact_timeout,
                    &cancel_token,
                )
                .await
                {
                    warn!("Failed to send SESS_TERM message: {e}");
                }
                return;
            }

            if buffer[5] & 0xFE != 0 {
                info!(
                    "Reserved flags {:#x} set in contact header from {addr}",
                    buffer[5]
                );
            }

            if config.use_tls && buffer[5] & 1 != 0 {
                // TLS!!
                todo!();
            } else {
                let _session = match session::Session::new_passive(
                    codec::MessageCodec::new_framed(stream),
                    cancel_token,
                )
                .await
                {
                    Err(e) => {
                        warn!("Failed to create session: {e}");
                        return;
                    }
                    Ok(s) => s,
                };
            }
        }
        Ok(Err(e)) => {
            error!("Failed to read contact header: {e}");
        }
        Err(_) => {
            warn!("Timeout reading contact header");
        }
    }
}

#[instrument(skip_all)]
async fn accept(config: Config, cancel_token: tokio_util::sync::CancellationToken) {
    let listener = tokio::net::TcpListener::bind(config.tcp_address)
        .await
        .trace_expect("Failed to bind TCP listener");

    info!("TCP server listening on {}", config.tcp_address);

    let mut task_set = tokio::task::JoinSet::new();

    loop {
        tokio::select! {
            r = listener.accept() => match r {
                Ok((stream, addr)) => {
                    let cancel_token_cloned = cancel_token.clone();
                    let config_cloned = config.clone();
                    task_set.spawn(new_contact(config_cloned,stream,addr, cancel_token_cloned));
                }
                Err(e) => {
                    warn!("Failed to accept connection: {e}");
                }
            },
            Some(r) = task_set.join_next() => r.trace_expect("Task terminated unexpectedly"),
            _ = cancel_token.cancelled() => break
        }
    }

    // Wait for all sub-tasks to complete
    while let Some(r) = task_set.join_next().await {
        r.trace_expect("Task terminated unexpectedly")
    }
}

#[instrument(skip_all)]
pub fn init(
    config: &config::Config,
    task_set: &mut tokio::task::JoinSet<()>,
    cancel_token: tokio_util::sync::CancellationToken,
) {
    let config = Config::new(config);
    if config.contact_timeout > 60 {
        warn!("RFC9174 specifies contact timeout SHOULD be a maximum of 60 seconds");
    }

    // Start listening
    task_set.spawn(accept(config, cancel_token));
}
