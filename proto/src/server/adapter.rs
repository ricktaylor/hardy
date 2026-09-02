// The server's two transfer adapters, one per direction, bridging the
// wire's chunked-transfer grammar and the BPA's [`Segment`] stream:
//
// - [`Reader`] adapts an incoming gRPC stream into a [`Receiver<Segment>`],
//   so the BPA consumes a wire transfer (the up half of a streamed Send
//   or Dispatch door) through the same pull interface as any other stream.
// - [`Writer`] adapts the other direction: it drains a delivery's segment
//   stream and pushes wire chunks onto a Receive response, honouring the
//   session cancel (the peer's in-band abandonment is raced at the call
//   site).
//
// The client's counterparts are `client::adapter::{Reader, Writer}`.

use hardy_async::{CancellationToken, sync::spin::Once};
use hardy_bpa::{
    async_trait,
    stream::{Receiver, RecvError, Segment},
};
use tokio::sync::mpsc::Sender;
use tonic::{Status, Streaming};
use tracing::debug;

use crate::stream::{Cancel, Chunk, chunks};

// -------------------------------------------------------------------
// Reader: wire -> Segment
// -------------------------------------------------------------------

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
                // Session death mid-transfer is a disconnect, not a
                // failed stream: UNAVAILABLE folds into the SDK's
                // Disconnected, matching the "BPA shuts down →
                // Disconnected" contract. Genuine truncation and failed
                // streams below stay ABORTED.
                self.status.call_once(|| Status::unavailable("Session closed"));
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

// -------------------------------------------------------------------
// Writer: Segment -> wire
// -------------------------------------------------------------------

// The response-side dual of [`Reader`]: it drains a delivery's segment
// stream and pushes wire chunks onto a Receive response, honouring the
// session cancel. Abandonment (the peer's in-band cancel) is raced
// against the whole drain at the call site, so the writer stays a plain
// segment-to-wire pump.
pub struct Writer<Resp> {
    tx: Sender<Result<Resp, Status>>,
    cancelled: CancellationToken,
}

impl<Resp: Chunk + Cancel + Send + 'static> Writer<Resp> {
    pub fn new(tx: Sender<Result<Resp, Status>>, cancelled: CancellationToken) -> Self {
        Self { tx, cancelled }
    }

    // Drains the delivery from `first` (the segment the door's probe
    // already pulled) then `stream`, emitting wire chunks until the final
    // segment. A withdrawn stream ends the response with the wire's in-band
    // cancel; the session cancel ends it with an aborted status. Terminal
    // statuses are try_send, best effort: they reach only a client still
    // reading, and awaiting a full channel past session death would
    // outlive pool shutdown.
    pub async fn write_all(self, first: Segment, mut stream: Box<dyn Receiver<Segment>>) {
        let Self { tx, cancelled } = self;
        let mut segment = first;
        loop {
            let last = matches!(segment, Segment::Final(_));
            for chunk in chunks(segment) {
                let permit = tokio::select! {
                    biased;
                    _ = cancelled.cancelled() => {
                        let _ = tx.try_send(Err(Status::aborted("Session closed")));
                        return;
                    }
                    permit = tx.reserve() => {
                        let Ok(permit) = permit else { return };
                        permit
                    }
                };
                permit.send(Ok(Resp::chunk(chunk)));
            }
            if last {
                return;
            }
            segment = tokio::select! {
                biased;
                _ = cancelled.cancelled() => {
                    let _ = tx.try_send(Err(Status::aborted("Session closed")));
                    return;
                }
                segment = stream.recv() => match segment {
                    Ok(segment) => segment,
                    // Withdrawn mid-collection (expiry, shutdown): the
                    // wire's in-band cancel, then a clean end.
                    Err(_) => {
                        let _ = tx.try_send(Ok(Resp::cancel()));
                        return;
                    }
                },
            };
        }
    }
}
