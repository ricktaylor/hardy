//! The two endpoint APIs, `hardy.application.v1` and `hardy.service.v1`, and
//! what they share.
//!
//! Both register an endpoint on the BPA and exchange bundles with it, one as
//! ADUs and one whole, so both end a collection the same way: with the `ack`
//! that commits the bundle or the `cancel` that leaves it parked.

pub mod application;
pub mod service;

use hardy_bpa::{
    services,
    stream::{Receiver, Segment},
};
use tonic::{Status, Streaming};
use tracing::{debug, warn};

use crate::{
    chunking::{BoundedChunkSender, MaxChunkSize},
    grammar::{Ack, Cancel, Chunk},
    server::{announce::Collection, services::is_disconnect},
    timeouts::{Stage, Timeouts},
};

/// What the client sent to end a collection.
///
/// A `cancel` is the client asking for the ending, not a fault, so the call it
/// arrives on ends `OK` and nothing is sent back. Keeping it on this side of
/// the result is what lets every genuine fault be a [`Status`] and nothing
/// else.
pub enum Completion {
    /// The client sent `ack`.
    Acked,
    /// The client sent `cancel`.
    Cancelled,
}

/// Waits for the `ack` or `cancel` that ends a collection.
///
/// Any other message is ignored; the first one is logged.
///
/// # Errors
///
/// Returns `CANCELLED` if the stream ends first, and `ABORTED` if it fails.
pub async fn wait_for_completion<R: Ack + Cancel>(
    requests: &mut Streaming<R>,
) -> Result<Completion, Status> {
    let mut warned = false;

    loop {
        match requests.message().await {
            Ok(Some(request)) if request.is_ack() => return Ok(Completion::Acked),
            Ok(Some(request)) if request.is_cancel() => {
                return Ok(Completion::Cancelled);
            }
            Ok(Some(_)) if !warned => {
                warned = true;
                warn!("ignoring unexpected message on the request stream");
            }
            Ok(Some(_)) => {}
            Ok(None) => return Err(Status::cancelled("request stream closed before the ack")),
            Err(error) => {
                debug!("request stream failed: {error}");
                return Err(Status::aborted("request stream failed"));
            }
        }
    }
}

/// Streams the bundle from `stream` to the client, then awaits the `ack` or
/// `cancel` that ends the call.
///
/// `collection` supplies the request stream, the response sender, and the
/// [`CollectionGuard`](crate::server::announce::CollectionGuard) holding the
/// slot reserved for the terminal message, so the call can always report how
/// it ended even if the client has filled the channel. The request stream is polled concurrently with the
/// transfer, since either message may arrive mid-stream; once the client has
/// taken the final chunk, [`Limits::idle`](crate::server::Limits::idle) bounds
/// the wait for the `ack`. The bundle goes out in chunks of at most
/// `max_chunk_size` bytes, the size the session negotiated. The session ending,
/// through `timeouts`, aborts the call.
///
/// An ending that is not the completed transfer abandons the transfer where
/// it stands, so `stream` is left drained as far as the client was told of.
/// Every such ending hands the bundle back to the BPA to be offered again,
/// which re-reads it. That includes this future being dropped mid-exchange,
/// which the guard reports to the client as `ABORTED`.
///
/// # Errors
///
/// Returns [`services::Error::Disconnected`] when the session itself is gone,
/// which is the session ending or one of its bounds elapsing, and
/// [`services::Error::StreamCancelled`] for every other failure; both tell the
/// BPA to keep the bundle. The terminal [`Status`] is sent through the
/// reserved slot first.
pub async fn deliver<Rsp: Chunk + Send + 'static, Req: Ack + Cancel>(
    collection: Collection<Rsp, Req>,
    stream: &mut dyn Receiver<Segment>,
    timeouts: &Timeouts,
    max_chunk_size: MaxChunkSize,
) -> services::Result<()> {
    let Collection {
        mut requests,
        responses_tx,
        guard,
    } = collection;

    let mut sender = BoundedChunkSender::new(responses_tx, timeouts, stream, max_chunk_size);
    // The send is polled before the request side, because the client's `ack`
    // and the send's own completion become ready together: the send ends by
    // waiting for the client to take the last chunk, which is what the client
    // acks. Reading the request side first would charge a prompt client with
    // an `ack` that in fact came after the chunk it acknowledges.
    let transfer = tokio::select! {
        biased;
        _ = timeouts.cancelled() => Err(Status::unavailable("registration closed")),
        sent = sender.send_all() => sent,
        result = wait_for_completion(&mut requests) => match result {
            Ok(Completion::Acked) => Err(Status::invalid_argument("ack before the last chunk")),
            // A cancel is the client asking for the ending, so the slot is
            // released unused and the stream closes `OK`.
            Ok(Completion::Cancelled) => {
                drop(guard.disarm());
                return Err(services::Error::StreamCancelled);
            }
            Err(status) => Err(status),
        },
    };

    let status = match transfer {
        Err(status) => status,
        Ok(()) => {
            let completion = timeouts
                .bound(Stage::Ack, wait_for_completion(&mut requests))
                .await;
            match completion {
                Ok(Completion::Acked) => {
                    drop(guard.disarm());
                    return Ok(());
                }
                Ok(Completion::Cancelled) => {
                    drop(guard.disarm());
                    return Err(services::Error::StreamCancelled);
                }
                Err(status) => status,
            }
        }
    };

    // The reserved slot guarantees room for this, whatever the client has
    // left queued.
    let error = if is_disconnect(&status) {
        services::Error::Disconnected
    } else {
        services::Error::StreamCancelled
    };
    guard.disarm().send(Err(status));
    Err(error)
}

#[cfg(test)]
mod tests {
    use core::{
        pin::pin,
        task::{Context, Waker},
    };
    use std::sync::Arc;

    use hardy_async::CancellationToken;
    use hardy_bpa::Bytes;
    use hardy_bpv7::{bundle::Id as BundleId, creation_timestamp::CreationTimestamp};
    use http_body::Frame;
    use http_body_util::StreamBody;
    use tokio_stream::{StreamExt, pending};
    use tonic::{Code, codec::Codec};
    use tonic_prost::ProstCodec;

    use super::*;
    use crate::{
        application::{ReceiveRequest, ReceiveResponse},
        server::{Limits, announce::Announcements},
    };

    // A request stream that stays open and silent: a client that never acks.
    fn silent() -> Streaming<ReceiveRequest> {
        Streaming::new_request(
            ProstCodec::<ReceiveRequest, ReceiveRequest>::default().decoder(),
            StreamBody::new(pending::<Result<Frame<Bytes>, Status>>()),
            None,
            None,
        )
    }

    #[tokio::test(start_paused = true)]
    async fn a_client_that_takes_the_last_chunk_and_never_acks_stalls_at_the_ack_stage() {
        let timeouts = Arc::new(Timeouts::new(Limits::default(), CancellationToken::new()));
        let announcements = Announcements::<ReceiveResponse, ReceiveRequest>::default();
        let bundle_id = BundleId {
            source: "ipn:1.1".parse().unwrap(),
            timestamp: CreationTimestamp::now(),
            fragment_info: None,
        };
        let mut announced = pin!(announcements.announce(&timeouts, &bundle_id, async || Ok(())));
        assert!(
            announced
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending(),
            "the announcement must wait to be collected"
        );
        let mut responses = announcements
            .collect(&bundle_id, silent())
            .expect("the bundle is announced");
        let collection = announced
            .await
            .expect("the collection must win the hand-over");

        let adu = Bytes::from_static(b"taken but never acked");
        let mut stream = adu.clone();
        let delivered = deliver(collection, &mut stream, &timeouts, MaxChunkSize::default());
        // The client takes every chunk the moment it is sent, and then says
        // nothing: the only bound left to miss is the one on its ack.
        let reader = async {
            let mut chunks = Vec::new();
            let mut ending = None;
            while let Some(message) = responses.next().await {
                match message {
                    Ok(chunk) => chunks.push(chunk),
                    Err(status) => ending = Some(status),
                }
            }
            (chunks, ending)
        };
        let (delivered, (chunks, ending)) = tokio::join!(delivered, reader);

        assert!(
            matches!(delivered, Err(services::Error::Disconnected)),
            "a stall closes the session, so the BPA must keep the bundle"
        );
        assert_eq!(chunks, [ReceiveResponse::chunk(Segment::Final(adu))]);
        let ending = ending.expect("the stall must be told to the client");
        assert_eq!(ending.code(), Code::DeadlineExceeded);
        assert_eq!(
            timeouts.timed_out().await,
            Stage::Ack,
            "the stall must be recorded against the ack stage"
        );
    }
}
