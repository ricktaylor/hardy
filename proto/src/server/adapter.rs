// The server's transfer adapters, bridging the wire's chunked grammar
// and the BPA's [`Segment`] stream. One writer serves every collecting
// door: the peer's answer always ends the transfer, so the surface
// races it against `write_all` at the call site. The client's
// counterparts are `client::adapter::{ResponseReader, RequestWriter}`.

use core::ops::ControlFlow;

use hardy_async::{CancellationToken, sync::spin::Once};
use hardy_bpa::{
    async_trait,
    stream::{Receiver, RecvError, Segment},
};
use tokio::sync::mpsc::Sender;
use tonic::{Status, Streaming};
use tracing::debug;

use crate::{
    grammar::{Cancel, Chunk},
    transfer::Writer,
};

// The wire's chunks as the segment stream the BPA pulls, so bundle
// bytes never materialise in the server. Any ending short of the last
// chunk fails the pull, and the reason recorded in
// [`status`](Self::status) becomes the call's terminal status.
pub struct RequestReader<M> {
    requests: Streaming<M>,
    cancelled: CancellationToken,
    status: Once<Status>,
    // The noun in this door's terminal statuses ("Send", "Dispatch").
    label: &'static str,
}

impl<M> RequestReader<M> {
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
impl<M: Chunk + Cancel + Send + 'static> Receiver<Segment> for RequestReader<M> {
    async fn recv(&mut self) -> Result<Segment, RecvError> {
        let message = tokio::select! {
            biased;
            _ = self.cancelled.cancelled() => {
                // A disconnect, not a failed stream: UNAVAILABLE folds
                // into the SDK's Disconnected. Truncation and failed
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

// Why a transfer ended before its last chunk, for the surface to
// translate into what it owes the BPA. The writer has already ended the
// response on both, so neither is owed anything more on the wire.
pub enum Interrupted {
    // The session died mid-transfer. The surface owes the BPA a
    // disconnection, not a failure.
    Session,
    // The BPA withdrew its stream, or the client stopped reading.
    // Either way the bundle did not land.
    Broken,
}

// The bundle as wire chunks on a collecting call's response, the dual of
// [`RequestReader`]. Terminal statuses are best-effort `try_send`:
// awaiting a full channel past session death would outlive pool
// shutdown. `write_all` returns `Continue` once the last chunk is on the
// wire; what completes the transfer past that is the surface's own wait.
pub struct ResponseWriter<'a, Rsp> {
    responses_tx: &'a Sender<Result<Rsp, Status>>,
    cancelled: &'a CancellationToken,
    stream: &'a mut dyn Receiver<Segment>,
}

impl<'a, Rsp> ResponseWriter<'a, Rsp> {
    pub fn new(
        responses_tx: &'a Sender<Result<Rsp, Status>>,
        cancelled: &'a CancellationToken,
        stream: &'a mut dyn Receiver<Segment>,
    ) -> Self {
        Self {
            responses_tx,
            cancelled,
            stream,
        }
    }

    // The session cancel arm, shared by both awaits. A disconnect, not
    // a failed stream: UNAVAILABLE folds into the SDK's Disconnected,
    // where ABORTED is for a transfer that broke on its own terms.
    fn session_closed(&self) -> Interrupted {
        let _ = self
            .responses_tx
            .try_send(Err(Status::unavailable("Session closed")));
        Interrupted::Session
    }
}

impl<Rsp: Chunk + Cancel + Send + 'static> Writer for ResponseWriter<'_, Rsp> {
    type Break = Interrupted;

    async fn next(&mut self) -> ControlFlow<Interrupted, Segment> {
        let segment = tokio::select! {
            biased;
            _ = self.cancelled.cancelled() => {
                return ControlFlow::Break(self.session_closed());
            }
            segment = self.stream.recv() => segment,
        };
        match segment {
            Ok(segment) => ControlFlow::Continue(segment),
            // Withdrawn mid-transfer: an in-band cancel, then a clean
            // end, so the peer does not act on a partial bundle.
            Err(_) => {
                let _ = self.responses_tx.try_send(Ok(Rsp::cancel()));
                ControlFlow::Break(Interrupted::Broken)
            }
        }
    }

    async fn write(&mut self, chunk: Segment) -> ControlFlow<Interrupted> {
        let permit = tokio::select! {
            biased;
            _ = self.cancelled.cancelled() => {
                return ControlFlow::Break(self.session_closed());
            }
            permit = self.responses_tx.reserve() => match permit {
                Ok(permit) => permit,
                // The client is gone.
                Err(_) => return ControlFlow::Break(Interrupted::Broken),
            },
        };
        permit.send(Ok(Rsp::chunk(chunk)));
        ControlFlow::Continue(())
    }
}
