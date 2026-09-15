//! The two endpoint APIs, `hardy.application.v1` and `hardy.service.v1`, and
//! what they share.
//!
//! Both register an endpoint on the BPA and exchange bundles with it, one as
//! ADUs and one whole, so both end a collection the same way: with the `ack`
//! that commits the bundle or the `cancel` that leaves it parked.

pub mod application;
pub mod service;

use hardy_async::CancellationToken;
use hardy_bpa::{
    services,
    stream::{Receiver, Segment},
};
use tonic::{Status, Streaming};
use tracing::{debug, warn};

use crate::{
    chunking::{BoundedChunkSender, ChunkSize, TransferError},
    grammar::{Ack, Cancel, Chunk},
    server::{
        announce::Collection,
        services::is_disconnect,
        watchdog::{Stage, Watchdog},
    },
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
            Ok(None) => return Err(Status::cancelled("request stream closed")),
            Err(e) => {
                debug!("request stream failed: {e}");
                return Err(Status::aborted("request stream failed"));
            }
        }
    }
}

/// Streams the bundle from `stream` to the client, then awaits the `ack` or
/// `cancel` that ends the call.
///
/// `collection` supplies the request stream, the response sender, and an
/// [`OwnedPermit`](tokio::sync::mpsc::OwnedPermit) reserved for the terminal
/// message, so the call can always report how it ended even if the client has
/// filled the channel. The request stream is polled concurrently with the
/// transfer, since either message may arrive mid-stream; once the client has
/// taken the final chunk, [`Watchdog::idle`] bounds the wait for the `ack`. The
/// bundle goes out in chunks of at most `chunk_size` bytes, the size the
/// session negotiated. `cancel` aborts the call when the registration ends.
///
/// # Errors
///
/// Returns [`services::Error::Disconnected`] when the session itself is gone,
/// which is `cancel` firing or a watchdog deadline elapsing, and
/// [`services::Error::StreamCancelled`] for every other failure; both tell the
/// BPA to keep the bundle. The terminal [`Status`] is sent on the reserved
/// permit first.
pub async fn deliver<Rsp: Chunk + Send + 'static, Req: Ack + Cancel>(
    collection: Collection<Rsp, Req>,
    stream: &mut dyn Receiver<Segment>,
    cancel: &CancellationToken,
    watchdog: &Watchdog,
    chunk_size: ChunkSize,
) -> services::Result<()> {
    let Collection {
        mut requests,
        responses_tx,
        permit,
    } = collection;

    let mut writer = BoundedChunkSender::new(responses_tx, watchdog.limits(), stream, chunk_size);
    let transfer = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(Status::unavailable("registration closed")),
        result = wait_for_completion(&mut requests) => match result {
            Ok(Completion::Acked) => Err(Status::invalid_argument("ack before the last chunk")),
            Ok(Completion::Cancelled) => return Err(services::Error::StreamCancelled),
            Err(status) => Err(status),
        },
        written = writer.write_all() => written.map_err(|error| match error {
            TransferError::Stalled => watchdog.stall(Stage::Drain),
            TransferError::Failed(status) => status,
        }),
    };

    let status = match transfer {
        Err(status) => status,
        Ok(()) => {
            let completion = tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(Status::unavailable("registration closed")),
                result = wait_for_completion(&mut requests) => result,
                stalled = watchdog.idle(Stage::Ack) => Err(stalled),
            };
            match completion {
                Ok(Completion::Acked) => return Ok(()),
                Ok(Completion::Cancelled) => return Err(services::Error::StreamCancelled),
                Err(status) => status,
            }
        }
    };

    // The reserved permit guarantees room for this, whatever the client has
    // left queued.
    let error = if is_disconnect(&status) {
        services::Error::Disconnected
    } else {
        services::Error::StreamCancelled
    };
    permit.send(Err(status));
    Err(error)
}
