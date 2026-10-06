//! The sending end of a chunked transfer.
//!
//! [`ChunkSender`] moves a segment stream onto the channel feeding a call, a
//! chunk at a time, for either end of the wire. [`BoundedChunkSender`] wraps
//! it for the server, holding the client reading the other end to the
//! session's bounds.

use hardy_bpa::stream::{Receiver, RecvError, Segment};
use tokio::sync::mpsc::{Sender, error::SendError};
#[cfg(feature = "server")]
use tonic::Status;

#[cfg(feature = "client")]
use crate::grammar::Cancel;
#[cfg(feature = "server")]
use crate::{
    chunking::{BUFFERED_CHUNKS, Budget, segment_len},
    timeouts::{Stage, Timeouts},
};
use crate::{
    chunking::{MaxChunkSize, chunks},
    grammar::Chunk,
};

/// Moves a BPA-side segment stream onto the channel feeding a call, as `chunk`
/// messages of at most the session's chunk size ended by `last_chunk`.
///
/// Nothing is sent after the first failure of either end: a stream that
/// ends before its final segment, or a channel whose call has answered or
/// been dropped.
pub struct ChunkSender<'a, M> {
    messages_tx: Sender<M>,
    stream: &'a mut dyn Receiver<Segment>,
    max_chunk_size: MaxChunkSize,
}

impl<'a, M: Chunk> ChunkSender<'a, M> {
    /// Creates a sender from `stream` onto `messages_tx`, chunking at the
    /// session's `max_chunk_size`.
    pub fn new(
        messages_tx: Sender<M>,
        stream: &'a mut dyn Receiver<Segment>,
        max_chunk_size: MaxChunkSize,
    ) -> Self {
        Self {
            messages_tx,
            stream,
            max_chunk_size,
        }
    }

    /// Pulls the next segment from the stream.
    ///
    /// # Errors
    ///
    /// Returns [`RecvError`] if the stream ends before its final segment.
    async fn pull(&mut self) -> Result<Segment, RecvError> {
        self.stream.recv().await
    }

    /// Splits `segment` into chunks of at most the session's chunk size.
    fn chunks(&self, segment: Segment) -> impl Iterator<Item = Segment> + use<M> {
        chunks(segment, self.max_chunk_size)
    }

    /// Sends one chunk, waiting for room on the channel.
    ///
    /// # Errors
    ///
    /// Returns the chunk back if the channel is closed, because the call has
    /// answered or been dropped.
    async fn send(&mut self, chunk: Segment) -> Result<(), SendError<M>> {
        self.messages_tx.send(M::chunk(chunk)).await
    }

    /// Waits until `depth` slots of the channel are free, which is when the
    /// call has taken everything but that many messages off it.
    ///
    /// # Errors
    ///
    /// Returns an error if the channel is closed.
    #[cfg(feature = "server")]
    async fn flushed(&mut self, depth: usize) -> Result<(), SendError<()>> {
        self.messages_tx.reserve_many(depth).await.map(drop)
    }
}

#[cfg(feature = "client")]
impl<M: Chunk + Cancel> ChunkSender<'_, M> {
    /// Sends `metadata` and then every chunk of the stream, returning once the
    /// transfer is sent, cancelled, or has nowhere left to go.
    ///
    /// If the stream fails before its final segment the sender sends a
    /// `cancel` so the server discards the partial transfer.
    pub async fn send_all(mut self, metadata: M) {
        if self.messages_tx.send(metadata).await.is_err() {
            return;
        }
        loop {
            let Ok(segment) = self.pull().await else {
                let _ = self.messages_tx.send(M::cancel()).await;
                return;
            };
            let last = matches!(segment, Segment::Final(_));
            for chunk in self.chunks(segment) {
                if self.send(chunk).await.is_err() {
                    return;
                }
            }
            if last {
                return;
            }
        }
    }
}

/// A [`ChunkSender`] onto the response channel of a `Receive` or `Forward`
/// call, which also holds the client reading it to the session's bounds.
///
/// The pull from the BPA is unbounded, because bounding it would charge the
/// client for the server's slowness. A stream that ends before its final
/// segment breaks the transfer with `ABORTED`, since the client must not act on
/// the partial bytes. Each send waits for room on the channel under the
/// `Drain` bound: a client that stops reading is stalled, and a client that
/// dropped the call has nothing left to tell.
///
/// The `Drain` bound is the transfer's [`Budget`]: a send only waits once the
/// channel is full, which is the client not reading, so a send the channel
/// takes at once costs nothing.
///
/// The channel is the one a collection opens: [`BUFFERED_CHUNKS`] chunk
/// slots and one more that the exchange holds for its ending, which the sender
/// never uses.
#[cfg(feature = "server")]
pub struct BoundedChunkSender<'a, Rsp> {
    inner: ChunkSender<'a, Result<Rsp, Status>>,
    budget: Budget<'a>,
    moved: u64,
}

#[cfg(feature = "server")]
impl<'a, Rsp: Chunk> BoundedChunkSender<'a, Rsp> {
    /// Creates a sender from `stream` onto `responses_tx`, chunking at the
    /// session's `max_chunk_size` and bounded by its `timeouts`. The transfer's
    /// clock starts now.
    pub fn new(
        responses_tx: Sender<Result<Rsp, Status>>,
        timeouts: &'a Timeouts,
        stream: &'a mut dyn Receiver<Segment>,
        max_chunk_size: MaxChunkSize,
    ) -> Self {
        Self {
            inner: ChunkSender::new(responses_tx, stream, max_chunk_size),
            budget: Budget::new(timeouts, Stage::Drain),
            moved: 0,
        }
    }

    /// Moves segments from the stream onto the channel until a
    /// [`Segment::Final`] has been sent and the client has taken every
    /// chunk off the channel.
    ///
    /// Returning only once the channel has flushed keeps the tail of the
    /// transfer under the `Drain` bound, which credits the bytes moved, rather
    /// than under whatever bound the caller applies to what the client owes
    /// after the last chunk.
    ///
    /// Dropping this future ends the transfer rather than pausing it: the
    /// sender has pulled bytes the client was never told of, and the chunk it
    /// was waiting to send goes with them, so a sender given up on must not
    /// be driven again. The bundle goes back to the BPA, which re-reads it.
    ///
    /// # Errors
    ///
    /// Fails with `ABORTED` if the stream ends before its final segment,
    /// `CANCELLED` if the client dropped the call, and `DEADLINE_EXCEEDED`,
    /// recorded at [`Stage::Drain`], if the client stopped reading.
    pub async fn send_all(&mut self) -> Result<(), Status> {
        loop {
            // The BPA's stream carries no reason, so none is stated.
            let segment = self
                .inner
                .pull()
                .await
                .map_err(|_| Status::aborted("the bundle was withdrawn"))?;
            let last = matches!(segment, Segment::Final(_));
            for chunk in self.inner.chunks(segment) {
                self.send(chunk).await?;
            }
            if last {
                return self.flush().await;
            }
        }
    }

    /// Sends one chunk, waiting for room under the `Drain` bound.
    async fn send(&mut self, chunk: Segment) -> Result<(), Status> {
        let sent = segment_len(&chunk);
        self.budget
            .spend(self.moved, self.inner.send(chunk))
            .await?
            .map_err(|_| Status::cancelled("the client dropped the call"))?;
        self.moved = self.moved.saturating_add(sent);

        Ok(())
    }

    /// Waits, under the `Drain` bound, until every chunk slot of the channel
    /// is free again, which is when the client has taken the last chunk.
    async fn flush(&mut self) -> Result<(), Status> {
        self.budget
            .spend(self.moved, self.inner.flushed(BUFFERED_CHUNKS))
            .await?
            .map_err(|_| Status::cancelled("the client dropped the call"))?;

        Ok(())
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use core::{iter::once, time::Duration};

    use hardy_async::CancellationToken;
    use hardy_bpa::{Bytes, async_trait, stream::RecvError};
    use tokio::sync::mpsc::{self, OwnedPermit};
    use tonic::Code;

    use super::*;
    use crate::{
        application::{ReceiveResponse, receive_response},
        limits::Limits,
    };

    type Channel = (
        mpsc::Sender<Result<ReceiveResponse, Status>>,
        mpsc::Receiver<Result<ReceiveResponse, Status>>,
    );

    // Opens a channel of the shape a collection opens.
    fn collection_channel() -> Channel {
        mpsc::channel(BUFFERED_CHUNKS + 1)
    }

    // Holds back the slot a collection keeps for its ending, which the sender
    // may not use.
    fn ending(
        responses_tx: &mpsc::Sender<Result<ReceiveResponse, Status>>,
    ) -> OwnedPermit<Result<ReceiveResponse, Status>> {
        responses_tx
            .clone()
            .try_reserve_owned()
            .expect("a freshly created channel has spare capacity")
    }

    // The timeouts of a session a client has already outlasted, so the first
    // wait a sender makes is one too many.
    fn stalling() -> Timeouts {
        Timeouts::new(
            Limits {
                idle: Duration::ZERO,
                grace: Duration::ZERO,
                ..Limits::default()
            },
            CancellationToken::new(),
        )
    }

    // The timeouts of a session with room to spare.
    fn unhurried() -> Timeouts {
        Timeouts::new(Limits::default(), CancellationToken::new())
    }

    // Asserts that `status` is what a sender the client outlasted breaks
    // with, and that `timeouts` recorded the stall at the drain stage.
    async fn assert_stalled_draining(status: &Status, timeouts: &Timeouts) {
        assert_eq!(status.code(), Code::DeadlineExceeded);
        assert_eq!(timeouts.timed_out().await, Stage::Drain);
    }

    struct EndedStream;

    #[async_trait]
    impl Receiver<Segment> for EndedStream {
        async fn recv(&mut self) -> Result<Segment, RecvError> {
            Err(RecvError)
        }
    }

    // Yields the segments in order, then ends.
    struct Segments(Vec<Segment>);

    #[async_trait]
    impl Receiver<Segment> for Segments {
        async fn recv(&mut self) -> Result<Segment, RecvError> {
            if self.0.is_empty() {
                Err(RecvError)
            } else {
                Ok(self.0.remove(0))
            }
        }
    }

    #[tokio::test]
    async fn a_stream_that_ends_early_aborts_the_transfer() {
        let (responses_tx, mut responses_rx) = collection_channel();
        let timeouts = unhurried();
        let mut stream = EndedStream;
        let mut sender = BoundedChunkSender::new(
            responses_tx.clone(),
            &timeouts,
            &mut stream,
            MaxChunkSize::default(),
        );

        let withdrawn = sender
            .send_all()
            .await
            .expect_err("a stream that ends early must break the transfer");
        assert_eq!(withdrawn.code(), Code::Aborted);

        assert!(
            responses_rx.try_recv().is_err(),
            "the status is the exchange's to send, through the slot it holds"
        );
    }

    #[tokio::test]
    async fn a_client_that_stops_reading_stalls_at_the_drain_stage() {
        let (responses_tx, mut responses_rx) = collection_channel();
        let _ending = ending(&responses_tx);
        let mut stream = Segments(
            (0..BUFFERED_CHUNKS)
                .map(|_| Segment::Next(Bytes::from_static(b"fits")))
                .chain(once(Segment::Final(Bytes::from_static(b"stalls"))))
                .collect(),
        );
        let timeouts = stalling();
        let mut sender = BoundedChunkSender::new(
            responses_tx.clone(),
            &timeouts,
            &mut stream,
            MaxChunkSize::default(),
        );

        let stalled = sender
            .send_all()
            .await
            .expect_err("a chunk with nowhere to go must break as stalled");
        assert_stalled_draining(&stalled, &timeouts).await;

        for _ in 0..BUFFERED_CHUNKS {
            let Some(Ok(ReceiveResponse {
                response: Some(receive_response::Response::Chunk(chunk)),
            })) = responses_rx.recv().await
            else {
                panic!("expected the sent chunk");
            };
            assert_eq!(chunk, Bytes::from_static(b"fits"));
        }

        assert!(
            responses_rx.try_recv().is_err(),
            "the stall is the exchange's to send, through the slot it holds"
        );
    }

    #[tokio::test]
    async fn a_client_that_leaves_the_tail_unread_stalls_at_the_drain_stage() {
        let (responses_tx, mut responses_rx) = collection_channel();
        let _ending = ending(&responses_tx);
        let mut stream = Segments(vec![Segment::Final(Bytes::from_static(b"tail"))]);
        let timeouts = stalling();
        let mut sender = BoundedChunkSender::new(
            responses_tx.clone(),
            &timeouts,
            &mut stream,
            MaxChunkSize::default(),
        );

        let stalled = sender
            .send_all()
            .await
            .expect_err("a last chunk nobody reads must break as stalled");
        assert_stalled_draining(&stalled, &timeouts).await;

        let Some(Ok(ReceiveResponse {
            response: Some(receive_response::Response::LastChunk(chunk)),
        })) = responses_rx.recv().await
        else {
            panic!("expected the sent last chunk");
        };
        assert_eq!(chunk, Bytes::from_static(b"tail"));
    }

    #[tokio::test]
    async fn a_transfer_completes_once_the_client_has_taken_the_last_chunk() {
        let (responses_tx, mut responses_rx) = collection_channel();
        let _ending = ending(&responses_tx);
        let mut stream = Segments(vec![
            Segment::Next(Bytes::from_static(b"head")),
            Segment::Final(Bytes::from_static(b"tail")),
        ]);
        let timeouts = unhurried();
        let mut sender = BoundedChunkSender::new(
            responses_tx.clone(),
            &timeouts,
            &mut stream,
            MaxChunkSize::default(),
        );

        let reader = tokio::spawn(async move {
            let mut chunks = Vec::new();
            for _ in 0..2 {
                let Some(Ok(ReceiveResponse {
                    response: Some(chunk),
                })) = responses_rx.recv().await
                else {
                    panic!("expected a chunk");
                };
                chunks.push(chunk);
            }
            (chunks, responses_rx)
        });

        assert!(matches!(sender.send_all().await, Ok(())));

        let (chunks, mut responses_rx) = reader.await.expect("the reader must not panic");
        assert_eq!(
            chunks,
            [
                receive_response::Response::Chunk(Bytes::from_static(b"head")),
                receive_response::Response::LastChunk(Bytes::from_static(b"tail")),
            ]
        );
        assert!(
            responses_rx.try_recv().is_err(),
            "nothing follows the last chunk on the sender's side"
        );
    }
}
