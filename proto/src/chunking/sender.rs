//! The sending end of a chunked transfer.
//!
//! [`ChunkSender`] moves a segment stream onto the channel feeding a call, a
//! chunk at a time, for either end of the wire. [`BoundedChunkSender`] wraps
//! it for the server, holding the client reading the other end to the
//! session's bounds.

#[cfg(feature = "server")]
use core::time::Duration;

use hardy_bpa::stream::{Receiver, RecvError, Segment};
use tokio::sync::mpsc::{Sender, error::SendError};
#[cfg(feature = "server")]
use tokio::time::{Instant, sleep_until};
#[cfg(feature = "server")]
use tonic::Status;

#[cfg(feature = "client")]
use crate::grammar::Cancel;
use crate::{
    chunking::{ChunkSize, chunks},
    grammar::Chunk,
};
#[cfg(feature = "server")]
use crate::{
    chunking::{TransferError, segment_len},
    server::{Limits, announce::DATA_CHANNEL_DEPTH},
};

/// Moves a BPA-side segment stream onto the channel feeding a call, as `chunk`
/// messages of at most the session's chunk size ended by `last_chunk`.
///
/// Nothing is written after the first failure of either end: a stream that
/// ends before its final segment, or a channel whose call has answered or
/// been dropped.
pub struct ChunkSender<'a, M> {
    messages_tx: Sender<M>,
    stream: &'a mut dyn Receiver<Segment>,
    chunk_size: ChunkSize,
}

impl<'a, M: Chunk> ChunkSender<'a, M> {
    /// Creates a sender from `stream` onto `messages_tx`, chunking at the
    /// session's `chunk_size`.
    pub fn new(
        messages_tx: Sender<M>,
        stream: &'a mut dyn Receiver<Segment>,
        chunk_size: ChunkSize,
    ) -> Self {
        Self {
            messages_tx,
            stream,
            chunk_size,
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
        chunks(segment, self.chunk_size)
    }

    /// Writes one chunk, waiting for room on the channel.
    ///
    /// # Errors
    ///
    /// Returns the chunk back if the channel is closed, because the call has
    /// answered or been dropped.
    async fn write(&mut self, chunk: Segment) -> Result<(), SendError<M>> {
        self.messages_tx.send(M::chunk(chunk)).await
    }

    /// Waits until `depth` slots of the channel are free, which is when the
    /// call has taken everything but that many messages off it.
    ///
    /// # Errors
    ///
    /// Returns an error if the channel is closed.
    #[cfg(feature = "server")]
    async fn drained(&mut self, depth: usize) -> Result<(), SendError<()>> {
        self.messages_tx.reserve_many(depth).await.map(drop)
    }
}

#[cfg(feature = "client")]
impl<M: Chunk + Cancel> ChunkSender<'_, M> {
    /// Writes `metadata` and then every chunk of the stream, returning once the
    /// transfer is written, cancelled, or has nowhere left to go.
    ///
    /// If the stream fails before its final segment the sender writes a
    /// `cancel` so the server discards the partial transfer.
    pub async fn write_all(mut self, metadata: M) {
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
                if self.write(chunk).await.is_err() {
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
/// the partial bytes. Each write waits for room on the channel under the
/// `Drain` bound: a client that stops reading is stalled, and a client that
/// dropped the call has nothing left to tell.
///
/// The `Drain` bound is the earlier of [`Limits::idle`] from the last chunk
/// taken and, measured from the start of the transfer, [`Limits::grace`] plus
/// the time the bytes moved so far have earned at [`Limits::min_rate`], so a
/// client reading one byte at a time cannot keep the transfer open by
/// resetting `idle`, and `idle` remains the ceiling on any single gap.
///
/// The channel is the one a collection opens: [`DATA_CHANNEL_DEPTH`] chunk
/// slots and one more that the exchange holds for its ending, which the sender
/// never uses.
#[cfg(feature = "server")]
pub struct BoundedChunkSender<'a, Rsp> {
    inner: ChunkSender<'a, Result<Rsp, Status>>,
    limits: Limits,
    started: Instant,
    moved: u64,
}

#[cfg(feature = "server")]
impl<'a, Rsp: Chunk> BoundedChunkSender<'a, Rsp> {
    /// Creates a sender from `stream` onto `responses_tx`, chunking at the
    /// session's `chunk_size` and bounded by `limits`. The transfer's clock
    /// starts now.
    pub fn new(
        responses_tx: Sender<Result<Rsp, Status>>,
        limits: Limits,
        stream: &'a mut dyn Receiver<Segment>,
        chunk_size: ChunkSize,
    ) -> Self {
        Self {
            inner: ChunkSender::new(responses_tx, stream, chunk_size),
            limits,
            started: Instant::now(),
            moved: 0,
        }
    }

    /// Moves segments from the stream onto the channel until a
    /// [`Segment::Final`] has been written and the client has taken every
    /// chunk off the channel.
    ///
    /// Returning only once the channel has drained keeps the tail of the
    /// transfer under the `Drain` bound, which credits the bytes moved, rather
    /// than under whatever bound the caller applies to what the client owes
    /// after the last chunk.
    ///
    /// # Errors
    ///
    /// Fails with `ABORTED` if the stream ends before its final segment and
    /// `CANCELLED` if the client dropped the call, and stalls if the client
    /// stopped reading.
    pub async fn write_all(&mut self) -> Result<(), TransferError> {
        loop {
            // The BPA's stream carries no reason, so none is stated.
            let segment = self
                .inner
                .pull()
                .await
                .map_err(|_| Status::aborted("the bundle was withdrawn"))?;
            let last = matches!(segment, Segment::Final(_));
            for chunk in self.inner.chunks(segment) {
                self.write(chunk).await?;
            }
            if last {
                return self.drain().await;
            }
        }
    }

    /// Sends one chunk, waiting for room under the `Drain` bound.
    async fn write(&mut self, chunk: Segment) -> Result<(), TransferError> {
        let written = segment_len(&chunk);
        let deadline = self.deadline();
        tokio::select! {
            biased;
            sent = self.inner.write(chunk) => {
                sent.map_err(|_| Status::cancelled("the client dropped the call"))?;
            }
            () = sleep_until(deadline) => return Err(TransferError::Stalled),
        }
        self.moved = self.moved.saturating_add(written);

        Ok(())
    }

    /// Waits, under the `Drain` bound, until every chunk slot of the channel
    /// is free again, which is when the client has taken the last chunk.
    async fn drain(&mut self) -> Result<(), TransferError> {
        let deadline = self.deadline();
        tokio::select! {
            biased;
            drained = self.inner.drained(DATA_CHANNEL_DEPTH) => {
                Ok(drained.map_err(|_| Status::cancelled("the client dropped the call"))?)
            }
            () = sleep_until(deadline) => Err(TransferError::Stalled),
        }
    }

    /// Returns when the client will have fallen behind the `Drain` bound if it
    /// takes nothing more.
    ///
    /// The deadline moves with every chunk, so it is read again after each.
    fn deadline(&self) -> Instant {
        let idle = Instant::now() + self.limits.idle;
        match self.limits.min_rate {
            Some(min_rate) => {
                let earned = Duration::from_secs(self.moved / min_rate.get());
                idle.min(self.started + self.limits.grace + earned)
            }
            None => idle,
        }
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use core::time::Duration;

    use hardy_bpa::{Bytes, async_trait, stream::RecvError};
    use tokio::sync::mpsc::{self, OwnedPermit};
    use tonic::Code;

    use super::*;
    use crate::application::{ReceiveResponse, receive_response};

    type Channel = (
        mpsc::Sender<Result<ReceiveResponse, Status>>,
        mpsc::Receiver<Result<ReceiveResponse, Status>>,
    );

    /// Opens a channel of the shape a collection opens.
    fn collection_channel() -> Channel {
        mpsc::channel(DATA_CHANNEL_DEPTH + 1)
    }

    /// Holds back the slot a collection keeps for its ending, which the writer
    /// may not use.
    fn ending(
        responses_tx: &mpsc::Sender<Result<ReceiveResponse, Status>>,
    ) -> OwnedPermit<Result<ReceiveResponse, Status>> {
        responses_tx
            .clone()
            .try_reserve_owned()
            .expect("a freshly created channel has spare capacity")
    }

    fn stalling_limits() -> Limits {
        Limits {
            idle: Duration::ZERO,
            grace: Duration::ZERO,
            ..Limits::default()
        }
    }

    struct NoStream;

    #[async_trait]
    impl Receiver<Segment> for NoStream {
        async fn recv(&mut self) -> Result<Segment, RecvError> {
            unreachable!("the test drives `write` directly");
        }
    }

    struct EndedStream;

    #[async_trait]
    impl Receiver<Segment> for EndedStream {
        async fn recv(&mut self) -> Result<Segment, RecvError> {
            Err(RecvError)
        }
    }

    /// Yields the segments in order, then ends.
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
        let mut stream = EndedStream;
        let mut writer = BoundedChunkSender::new(
            responses_tx.clone(),
            Limits::default(),
            &mut stream,
            ChunkSize::default(),
        );

        let Err(TransferError::Failed(withdrawn)) = writer.write_all().await else {
            panic!("a stream that ends early must break the transfer");
        };
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
        let mut stream = NoStream;
        let mut writer = BoundedChunkSender::new(
            responses_tx.clone(),
            stalling_limits(),
            &mut stream,
            ChunkSize::default(),
        );

        for _ in 0..DATA_CHANNEL_DEPTH {
            assert!(
                matches!(
                    writer
                        .write(Segment::Next(Bytes::from_static(b"fits")))
                        .await,
                    Ok(())
                ),
                "a chunk with room for it must not reach the watchdog"
            );
        }

        let Err(TransferError::Stalled) = writer
            .write(Segment::Final(Bytes::from_static(b"stalls")))
            .await
        else {
            panic!("a chunk with nowhere to go must break as stalled");
        };

        for _ in 0..DATA_CHANNEL_DEPTH {
            let Some(Ok(ReceiveResponse {
                response: Some(receive_response::Response::Chunk(chunk)),
            })) = responses_rx.recv().await
            else {
                panic!("expected the written chunk");
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
        let mut writer = BoundedChunkSender::new(
            responses_tx.clone(),
            stalling_limits(),
            &mut stream,
            ChunkSize::default(),
        );

        let Err(TransferError::Stalled) = writer.write_all().await else {
            panic!("a last chunk nobody reads must break as stalled");
        };

        let Some(Ok(ReceiveResponse {
            response: Some(receive_response::Response::LastChunk(chunk)),
        })) = responses_rx.recv().await
        else {
            panic!("expected the written last chunk");
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
        let mut writer = BoundedChunkSender::new(
            responses_tx.clone(),
            Limits::default(),
            &mut stream,
            ChunkSize::default(),
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

        assert!(matches!(writer.write_all().await, Ok(())));

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
            "nothing follows the last chunk on the writer's side"
        );
    }
}
