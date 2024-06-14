use super::*;
use futures::sink::SinkExt;
use futures::stream::StreamExt;
use serde::de::Unexpected;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
}

const KEEPALIVE: u64 = 60;

pub struct Session<T>
where
    T: futures::StreamExt<Item = Result<codec::Message, codec::Error>>
        + futures::SinkExt<codec::Message>
        + std::marker::Unpin,
    session::Error: From<<T as futures::Sink<codec::Message>>::Error>,
{
    transport: T,
    cancel_token: tokio_util::sync::CancellationToken,
}

impl<T> Session<T>
where
    T: futures::StreamExt<Item = Result<codec::Message, codec::Error>>
        + futures::SinkExt<codec::Message>
        + std::marker::Unpin,
    session::Error: From<<T as futures::Sink<codec::Message>>::Error>,
{
    pub async fn new_passive(
        mut transport: T,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<Self, Error> {
        // Read the SESS_INIT reply message with timeout
        let init = loop {
            match next_with_timeout(&mut transport, KEEPALIVE.saturating_mul(2), &cancel_token)
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

        Ok(Self {
            transport,
            cancel_token,
        })
    }
}

pub async fn next_with_timeout<T>(
    transport: &mut T,
    timeout: u64,
    cancel_token: &tokio_util::sync::CancellationToken,
) -> Result<codec::Message, Error>
where
    T: futures::StreamExt<Item = Result<codec::Message, codec::Error>> + std::marker::Unpin,
{
    // Read the SESS_INIT reply message with timeout
    tokio::select! {
        r = tokio::time::timeout(
            tokio::time::Duration::from_secs(timeout),
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
    timeout: u64,
    cancel_token: &tokio_util::sync::CancellationToken,
) -> Result<(), Error>
where
    T: futures::StreamExt<Item = Result<codec::Message, codec::Error>>
        + futures::SinkExt<codec::Message>
        + std::marker::Unpin,
    session::Error: From<<T as futures::Sink<codec::Message>>::Error>,
{
    // Send the SESS_TERM message
    transport.send(codec::Message::SessionTerm(msg)).await?;

    // Read the SESS_TERM reply message with timeout
    loop {
        match session::next_with_timeout(transport, timeout, cancel_token).await? {
            codec::Message::SessionTerm(codec::SessionTermMessage {
                message_flags: codec::SessionTermMessageFlags { reply: true },
                reason_code: codec::SessionTermReasonCode::VersionMismatch,
            }) => {
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
