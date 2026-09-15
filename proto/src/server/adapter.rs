// Server-side adapters between the wire's chunk messages and the
// BPA's `Segment` stream.

use hardy_async::sync::spin::Once;
use hardy_bpa::{
    async_trait,
    stream::{Receiver, RecvError, Segment},
};
use tokio::sync::mpsc::Sender;
use tonic::{Status, Streaming};
use tracing::debug;

use crate::{
    grammar::{Cancel, Chunk},
    server::{
        error::{self, Error},
        leases::{Lease, Leases},
    },
    transfer::Writer,
};

// Presents an inbound chunk stream as the segment receiver the BPA
// pulls. The first failure is recorded in `status`.
pub struct RequestReader<M> {
    requests: Streaming<M>,
    leases: Leases,
    status: Once<Status>,
    // The noun used in terminal statuses: "Send" or "Dispatch".
    label: &'static str,
}

impl<M> RequestReader<M> {
    pub fn new(requests: Streaming<M>, leases: Leases, label: &'static str) -> Self {
        Self {
            requests,
            leases,
            status: Once::new(),
            label,
        }
    }

    // The wire-side reason the transfer ended.
    pub fn status(&self) -> Option<Status> {
        self.status.get().cloned()
    }
}

#[async_trait]
impl<M: Chunk + Cancel + Send + 'static> Receiver<Segment> for RequestReader<M> {
    async fn recv(&mut self) -> Result<Segment, RecvError> {
        let message = tokio::select! {
            biased;
            _ = self.leases.cancelled() => {
                // The SDK maps UNAVAILABLE to `Disconnected`.
                self.status.call_once(|| {
                    Status::unavailable("The session is closed, so this call was abandoned")
                });
                return Err(RecvError);
            }
            message = self.requests.message() => message,
            expired = self.leases.expired(Lease::Feed) => {
                self.status.call_once(|| {
                    expired
                        .status()
                        .unwrap_or_else(|| Status::deadline_exceeded("The transfer stalled"))
                });
                return Err(RecvError);
            }
        };
        match message {
            Ok(Some(msg)) if msg.is_cancel() => {
                self.status.call_once(|| {
                    Status::cancelled(format!("{} cancelled by the client", self.label))
                });
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
            Ok(None) => {
                self.status.call_once(|| {
                    Status::aborted(
                        "The transfer ended without its last chunk, so nothing was submitted",
                    )
                });
                Err(RecvError)
            }
            Err(e) => {
                debug!("{} stream failed: {e}", self.label);
                self.status
                    .call_once(|| Status::aborted(format!("The {} stream failed", self.label)));
                Err(RecvError)
            }
        }
    }
}

// Writes a bundle as wire chunks on a collecting call's response
// stream. Terminal statuses are sent best-effort with `try_send`.
pub struct ResponseWriter<'a, Rsp> {
    responses_tx: &'a Sender<Result<Rsp, Status>>,
    leases: &'a Leases,
    stream: &'a mut dyn Receiver<Segment>,
}

impl<'a, Rsp> ResponseWriter<'a, Rsp> {
    pub fn new(
        responses_tx: &'a Sender<Result<Rsp, Status>>,
        leases: &'a Leases,
        stream: &'a mut dyn Receiver<Segment>,
    ) -> Self {
        Self {
            responses_tx,
            leases,
            stream,
        }
    }
}

impl<Rsp: Chunk + Cancel + Send + 'static> Writer for ResponseWriter<'_, Rsp> {
    type Error = Error;

    async fn next(&mut self) -> error::Result<Segment> {
        let segment = tokio::select! {
            biased;
            _ = self.leases.cancelled() => {
                if let Some(status) = Error::SessionClosed.status() {
                    let _ = self.responses_tx.try_send(Err(status));
                }
                return Err(Error::SessionClosed);
            }
            segment = self.stream.recv() => segment,
        };
        match segment {
            Ok(segment) => Ok(segment),
            // Send an in-band cancel so the peer discards the partial bundle.
            Err(_) => {
                let _ = self.responses_tx.try_send(Ok(Rsp::cancel()));
                Err(Error::TransferTruncated)
            }
        }
    }

    async fn write(&mut self, chunk: Segment) -> error::Result<()> {
        let permit = tokio::select! {
            biased;
            _ = self.leases.cancelled() => {
                if let Some(status) = Error::SessionClosed.status() {
                    let _ = self.responses_tx.try_send(Err(status));
                }
                return Err(Error::SessionClosed);
            }
            reserved = self.responses_tx.reserve() => reserved.map_err(|_| Error::CallDropped)?,
            expired = self.leases.expired(Lease::Drain) => {
                // The channel is full, so the status may be dropped.
                if let Some(status) = expired.status() {
                    let _ = self.responses_tx.try_send(Err(status));
                }
                return Err(expired);
            }
        };
        permit.send(Ok(Rsp::chunk(chunk)));

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use hardy_async::CancellationToken;
    use hardy_bpa::Bytes;
    use tokio::sync::mpsc;

    use super::*;
    use crate::{application::ReceiveResponse, server::Limits};

    // The tests drive `write` directly; `next` is never reached.
    struct NoStream;

    #[async_trait]
    impl Receiver<Segment> for NoStream {
        async fn recv(&mut self) -> Result<Segment, RecvError> {
            unreachable!("the test drives `write` directly");
        }
    }

    #[tokio::test]
    async fn a_client_that_stops_draining_loses_its_drain_lease() {
        let (responses_tx, mut responses_rx) = mpsc::channel::<Result<ReceiveResponse, Status>>(1);
        let mut stream = NoStream;
        // A zero drain lease: an already-ready `reserve` still wins the
        // race, so this is deterministic without a clock.
        let leases = Leases::new(
            CancellationToken::new(),
            "test",
            Limits {
                drain: Duration::ZERO,
                ..Limits::default()
            },
        );
        let mut writer = ResponseWriter::new(&responses_tx, &leases, &mut stream);

        assert!(
            matches!(
                writer
                    .write(Segment::Next(Bytes::from_static(b"fits")))
                    .await,
                Ok(())
            ),
            "a chunk with room for it must not spend the lease"
        );

        // The one slot is full, so this chunk must wait.
        assert!(
            matches!(
                writer
                    .write(Segment::Final(Bytes::from_static(b"stalls")))
                    .await,
                Err(Error::LeaseExpired(Lease::Drain))
            ),
            "a chunk with nowhere to go must break as stalled"
        );

        // The chunk that fit is intact.
        assert!(matches!(
            responses_rx.recv().await,
            Some(Ok(ReceiveResponse { .. }))
        ));
        assert!(responses_rx.try_recv().is_err());
    }
}
