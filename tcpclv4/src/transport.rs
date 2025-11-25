use super::*;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Peer closed the connection")]
    Hangup,

    #[error("Timed out waiting for message from peer")]
    Timeout,

    #[error("Cancelled")]
    Cancelled,

    #[error("The peer is not a TCPCLv4 speaker")]
    InvalidProtocol,

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Codec(#[from] codec::Error),
}

#[cfg_attr(feature = "tracing", instrument(skip(transport, cancel_token)))]
pub async fn terminate<T>(
    mut transport: T,
    reason_code: codec::SessionTermReasonCode,
    timeout: u16,
    cancel_token: &tokio_util::sync::CancellationToken,
) where
    T: futures::StreamExt<Item = Result<codec::Message, codec::Error>>
        + futures::SinkExt<codec::Message>
        + std::marker::Unpin,
    <T as futures::Sink<codec::Message>>::Error: std::fmt::Debug,
{
    let msg = codec::SessionTermMessage {
        reason_code,
        ..Default::default()
    };
    let mut expected_reply = msg.clone();
    expected_reply.message_flags.reply = true;

    // Send the SESS_TERM message
    if transport
        .send(codec::Message::SessionTerm(msg))
        .await
        .inspect_err(|e| {
            info!("Failed to send session terminate message: {e:?}");
        })
        .is_ok()
    {
        // Read the SESS_TERM reply message with timeout
        loop {
            match next_with_timeout(&mut transport, timeout, cancel_token).await {
                Err(e) => {
                    info!("Failed to read next message: {e:?}");
                    break;
                }
                Ok(codec::Message::SessionTerm(mut msg)) => {
                    if !msg.message_flags.reply {
                        // Terminations pass in the night...
                        msg.message_flags.reply = true;
                        transport
                            .send(codec::Message::SessionTerm(msg))
                            .await
                            .unwrap_or_else(|e| {
                                info!("Failed to send termination message to peer: {e:?}");
                            });
                    } else if msg != expected_reply {
                        info!(
                            "Mismatched SESS_TERM message: {:?}, expected {:?}",
                            msg, expected_reply
                        );
                    }
                    break;
                }
                Ok(msg) => {
                    info!("Unexpected message while waiting for SESS_TERM reply: {msg:?}");

                    // Send a MSG_REJECT/Unexpected message
                    if let Err(e) = transport
                        .send(codec::Message::Reject(codec::MessageRejectMessage {
                            reason_code: codec::MessageRejectionReasonCode::Unexpected,
                            rejected_message: msg.message_type() as u8,
                        }))
                        .await
                    {
                        info!("Failed to send rejection message to peer: {e:?}");
                        break;
                    }
                }
            }
        }
    }

    transport.close().await.unwrap_or_else(|e| {
        info!("Failed to cleanly close transport: {e:?}");
    })
}

#[cfg_attr(feature = "tracing", instrument(skip(transport, cancel_token)))]
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
