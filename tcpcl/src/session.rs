use super::*;
use thiserror::Error;
use tokio::sync::mpsc::*;
use utils::settings;

#[derive(Error, Debug)]
pub enum Error {
    #[error(transparent)]
    Codec(#[from] codec::Error),

    #[error("Remote end closed")]
    Hangup,

    #[error("Timeout")]
    Timeout,

    #[error("Cancelled")]
    Cancelled,

    #[error("Invalid contact header")]
    InvalidContactHeader,

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

const DEFAULT_KEEPALIVE_INTERVAL: u16 = 60;
const DEFAULT_SEGMENT_MRU: u64 = 16384;
const DEFAULT_TRANSFER_MRU: u64 = 0x4000_0000; // 4GiB

#[derive(Clone)]
pub struct Config {
    pub keepalive_interval: u16,
    pub segment_mru: u64,
    pub transfer_mru: u64,
    pub node_id: Option<bpv7::Eid>,
}

impl Config {
    pub fn new(config: &config::Config) -> Self {
        let config = Self {
            keepalive_interval: settings::get_with_default(
                config,
                "keepalive_interval",
                DEFAULT_KEEPALIVE_INTERVAL,
            )
            .trace_expect("Invalid 'keepalive_interval' value in configuration"),
            segment_mru: settings::get_with_default(config, "segment_mru", DEFAULT_SEGMENT_MRU)
                .trace_expect("Invalid 'segment_mru' value in configuration"),
            transfer_mru: settings::get_with_default(config, "transfer_mru", DEFAULT_TRANSFER_MRU)
                .trace_expect("Invalid 'transfer_mru' value in configuration"),
            node_id: settings::get_with_default::<String, _>(config, "node_id", String::new())
                .map(|s| {
                    if s.is_empty() {
                        None
                    } else {
                        Some(
                            s.parse::<bpv7::Eid>()
                                .trace_expect("Invalid 'node_id' value in configuration"),
                        )
                    }
                })
                .trace_expect("Invalid 'node_id' value in configuration"),
        };

        if config.keepalive_interval == 0 {
            info!("Session keepalive disabled");
        }

        if let Some(node_id) = &config.node_id {
            match node_id {
                bpv7::Eid::Ipn2 { .. } | bpv7::Eid::Ipn3 { .. } => {}
                bpv7::Eid::Dtn { node_name, .. } if !node_name.starts_with('~') => {}
                _ => {
                    // Fatal!
                    error!("Invalid 'node_id' value in configuration: {node_id}");
                    panic!("Invalid 'node_id' value in configuration: {node_id}");
                }
            }
        }
        config
    }
}

pub async fn new_passive<T>(
    config: Config,
    mut transport: T,
    cancel_token: tokio_util::sync::CancellationToken,
) -> Result<(), Error>
where
    T: futures::StreamExt<Item = Result<codec::Message, codec::Error>>
        + futures::SinkExt<codec::Message>
        + std::marker::Unpin,
    session::Error: From<<T as futures::Sink<codec::Message>>::Error>,
{
    // Read the SESS_INIT message with timeout
    let peer_init = loop {
        match next_with_timeout(&mut transport, config.keepalive_interval * 2, &cancel_token)
            .await?
        {
            codec::Message::SessionInit(init) => break init,
            m => {
                warn!("Unexpected message while waiting for SESS_INIT: {m:?}");

                // Send a MSG_REJECT/Unexpected message
                transport
                    .send(codec::Message::Reject(codec::MessageRejectMessage {
                        reason_code: codec::MessageRejectionReasonCode::Unexpected,
                        rejected_message: m.into(),
                    }))
                    .await?;
            }
        };
    };

    // Send our SESS_INIT message
    transport
        .send(codec::Message::SessionInit(codec::SessionInitMessage {
            keepalive_interval: config.keepalive_interval,
            segment_mru: config.segment_mru,
            transfer_mru: config.transfer_mru,
            node_id: config.node_id.clone(),
            ..Default::default()
        }))
        .await?;

    // TODO: Negotiate

    let keepalive_interval = peer_init.keepalive_interval.min(config.keepalive_interval);

    // If we have a peer node id, then we can forward
    let egress = peer_init.node_id.map(|_| {
        let (send_request, recv_request) = unbounded_channel::<Vec<u8>>();
        let (send_response, recv_response) = unbounded_channel::<tonic::Status>();
        (
            connection::new_client(send_request, recv_response),
            recv_request,
            send_response,
        )
    });

    if keepalive_interval > 0 {
        // TODO - Start keepalive timer
    }

    // TODO - Register Neighbour EID

    // And poll the connection

    Ok(())
}

pub async fn next_with_timeout<T>(
    transport: &mut T,
    timeout: u16,
    cancel_token: &tokio_util::sync::CancellationToken,
) -> Result<codec::Message, Error>
where
    T: futures::StreamExt<Item = Result<codec::Message, codec::Error>> + std::marker::Unpin,
{
    // Read the next message with timeout
    tokio::select! {
        r = tokio::time::timeout(
            tokio::time::Duration::from_secs(timeout as u64),
            transport.next(),
        ) => match r {
            Ok(Some(Ok(m))) => {
                Ok(m)
            }
            Ok(Some(Err(e))) => {
                Err(e.into())
            }
            Ok(None) => Err(Error::Hangup),
            Err(_) => Err(Error::Timeout)
        },
        _ = cancel_token.cancelled() => Err(Error::Cancelled)
    }
}

pub async fn send_session_term<T>(
    transport: &mut T,
    msg: codec::SessionTermMessage,
    timeout: u16,
    cancel_token: &tokio_util::sync::CancellationToken,
) -> Result<(), Error>
where
    T: futures::StreamExt<Item = Result<codec::Message, codec::Error>>
        + futures::SinkExt<codec::Message>
        + std::marker::Unpin,
    session::Error: From<<T as futures::Sink<codec::Message>>::Error>,
{
    let mut expected_reply = msg.clone();
    expected_reply.message_flags.reply = true;

    // Send the SESS_TERM message
    transport.send(codec::Message::SessionTerm(msg)).await?;

    // Read the SESS_TERM reply message with timeout
    loop {
        match session::next_with_timeout(transport, timeout, cancel_token).await? {
            codec::Message::SessionTerm(msg) if msg == expected_reply => {
                // Graceful shutdown - Check this call shutdown()
                return transport.close().await.map_err(Into::into);
            }
            m => {
                // TODO:  See https://www.rfc-editor.org/rfc/rfc9174.html#name-session-termination-message

                warn!("Unexpected message while waiting for SESS_TERM reply: {m:?}");

                // Send a MSG_REJECT/Unexpected message
                transport
                    .send(codec::Message::Reject(codec::MessageRejectMessage {
                        reason_code: codec::MessageRejectionReasonCode::Unexpected,
                        rejected_message: m.into(),
                    }))
                    .await?;
            }
        }
    }
}
