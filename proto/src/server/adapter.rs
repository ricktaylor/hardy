// The server's Reader: adapts an incoming gRPC stream into a
// [`Receiver<Segment>`], so the BPA consumes a wire transfer (the up
// half of a streamed Send or Dispatch door) through the same pull
// interface as any other stream. The read counterpart of the client's
// `adapter::Reader`; the server has no matching `Writer`, because it
// never pushes a transfer blindly: its writes (the Receive delivery
// pump, the Forward door) are RPC engines that race a control channel
// between chunks, and encode through the shared [`crate::stream::chunks`]
// re-framer.

use hardy_async::{CancellationToken, sync::spin::Once};
use hardy_bpa::{
    async_trait,
    stream::{Receiver, RecvError, Segment},
};
use tonic::{Status, Streaming};
use tracing::debug;

use crate::stream::{Cancel, Chunk};

// The wire's chunks become the segment stream the BPA pulls, so bundle
// bytes flow to the BPA without materialising in the bridge. The wire's
// last chunk is the final segment; every other ending (cancel,
// truncation, a failed stream, session death) fails the pull, and the
// reason recorded in [`status`](Self::status) becomes the call's
// terminal status, so the BPA only ever sees a cancelled stream.
pub struct Reader<M> {
    requests: Streaming<M>,
    cancelled: CancellationToken,
    status: Once<Status>,
    // The noun in this door's terminal statuses ("Send", "Dispatch").
    label: &'static str,
}

impl<M> Reader<M> {
    pub fn new(requests: Streaming<M>, cancelled: CancellationToken, label: &'static str) -> Self {
        Self {
            requests,
            cancelled,
            status: Once::new(),
            label,
        }
    }

    // The wire-side reason the transfer ended, which outranks the
    // generic stream error the BPA saw.
    pub fn status(&self) -> Option<Status> {
        self.status.get().cloned()
    }
}

#[async_trait]
impl<M: Chunk + Cancel + Send + 'static> Receiver<Segment> for Reader<M> {
    async fn recv(&mut self) -> Result<Segment, RecvError> {
        let message = tokio::select! {
            biased;
            _ = self.cancelled.cancelled() => {
                self.status.call_once(|| Status::aborted("Session closed"));
                return Err(RecvError);
            }
            message = self.requests.message() => message,
        };
        match message {
            Ok(Some(msg)) if msg.is_cancel() => {
                self.status
                    .call_once(|| Status::cancelled(format!("{} cancelled", self.label)));
                Err(RecvError)
            }
            Ok(Some(msg)) => match msg.into_chunk() {
                Some(segment) => Ok(segment),
                None => {
                    self.status.call_once(|| {
                        Status::invalid_argument("Messages after the first must be chunks")
                    });
                    Err(RecvError)
                }
            },
            // The last chunk is the commit signal: a stream ending
            // without it was truncated, and nothing is submitted.
            Ok(None) => {
                self.status
                    .call_once(|| Status::aborted("The transfer ended without its last chunk"));
                Err(RecvError)
            }
            Err(e) => {
                debug!("{} stream failed: {e}", self.label);
                self.status
                    .call_once(|| Status::aborted(format!("{} stream failed", self.label)));
                Err(RecvError)
            }
        }
    }
}
