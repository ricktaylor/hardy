//! The receiving end of a chunked transfer.
//!
//! [`ChunkReceiver`] turns the messages of a call into the segments of a
//! transfer, for either end of the wire. [`BoundedChunkReceiver`] wraps any
//! segment source for the server, holding the client to the session's bounds
//! and keeping the [`TransferError`] that ended the transfer.

#[cfg(feature = "server")]
use core::time::Duration;

use hardy_async::CancellationToken;
use hardy_bpa::{
    async_trait,
    stream::{Receiver, RecvError, Segment},
};
#[cfg(feature = "server")]
use tokio::time::{Instant, sleep_until};
use tonic::{Status, Streaming};
use tracing::debug;

use crate::grammar::Chunk;
#[cfg(feature = "server")]
use crate::{
    MAX_TRANSFER_SIZE,
    chunking::{ChunkSize, TransferError, segment_len},
    grammar::Cancel,
    server::Limits,
};

/// What ended a transfer short of its next segment.
enum Interruption<M> {
    /// The session ended.
    SessionClosed,
    /// The stream carried a message that is not a chunk.
    Unexpected(M),
    /// The stream closed before its last chunk.
    StreamClosed,
    /// The stream failed.
    StreamFailed(Status),
}

/// A [`Receiver`] over the chunk messages of a call, handed to the component
/// or the BPA as the transfer's segment source.
///
/// Each `chunk` or `last_chunk` message becomes one segment. Anything else ends
/// the transfer: the stream closing before its last chunk, a status, the
/// session ending, or a message that is not a chunk. Through [`Receiver`] all
/// of them are a bare [`RecvError`], which is what a component reads as a
/// truncated transfer: it must not act on the partial bytes. Ending the
/// transfer through the stream, rather than by dropping the reader's future,
/// is what the in-process BPA does too, so a component unwinds the same way on
/// either side of the wire.
pub struct ChunkReceiver<'a, M> {
    messages: &'a mut Streaming<M>,
    cancel: &'a CancellationToken,
    interruption: Option<Interruption<M>>,
}

impl<'a, M: Chunk> ChunkReceiver<'a, M> {
    /// Creates a receiver over `messages` for the session that `cancel` ends.
    pub fn new(messages: &'a mut Streaming<M>, cancel: &'a CancellationToken) -> Self {
        Self {
            messages,
            cancel,
            interruption: None,
        }
    }
}

#[cfg(feature = "server")]
impl<M: Chunk + Cancel> ChunkReceiver<'_, M> {
    /// Returns the status a `Send` or `Dispatch` call ends with when the
    /// transfer was interrupted, consuming the receiver. Returns `None` if the
    /// transfer was not interrupted.
    pub fn into_error(self) -> Option<Status> {
        self.interruption.map(|interruption| match interruption {
            Interruption::SessionClosed => Status::unavailable("registration closed"),
            Interruption::Unexpected(message) if message.is_cancel() => {
                Status::cancelled("transfer cancelled")
            }
            Interruption::Unexpected(_) => Status::invalid_argument("expected a chunk or a cancel"),
            Interruption::StreamClosed => {
                Status::aborted("request stream closed before the last chunk")
            }
            Interruption::StreamFailed(_) => Status::aborted("request stream failed"),
        })
    }
}

#[async_trait]
impl<M: Chunk + Send + 'static> Receiver<Segment> for ChunkReceiver<'_, M> {
    async fn recv(&mut self) -> Result<Segment, RecvError> {
        let message = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(Interruption::SessionClosed),
            message = self.messages.message() => match message {
                Ok(Some(message)) => message.into_chunk().map_err(Interruption::Unexpected),
                Ok(None) => Err(Interruption::StreamClosed),
                Err(status) => Err(Interruption::StreamFailed(status)),
            },
        };
        message.map_err(|interruption| {
            match &interruption {
                Interruption::SessionClosed => debug!("the session ended during the transfer"),
                Interruption::Unexpected(_) => {
                    debug!("the transfer carried a message that is not a chunk")
                }
                Interruption::StreamClosed => {
                    debug!("the transfer stream ended before its last chunk")
                }
                Interruption::StreamFailed(status) => debug!("transfer stream failed: {status}"),
            }
            self.interruption.get_or_insert(interruption);
            RecvError
        })
    }
}

/// A [`Receiver`] over another, which also holds the client feeding it to the
/// session's bounds.
///
/// Besides whatever ends the inner receiver, a chunk above the size the
/// session negotiated, the client falling behind the `Feed` bound, or the
/// bytes disagreeing with a declared size end the transfer. The BPA sees only
/// [`RecvError`]; how the transfer broke is kept for
/// [`into_error`](BoundedChunkReceiver::into_error), as the inner receiver
/// keeps its own ending.
///
/// The `Feed` bound is the earlier of [`Limits::idle`] from the last segment
/// and, measured from the start of the transfer, [`Limits::grace`] plus the
/// time the bytes moved so far have earned at [`Limits::min_rate`]. Time is
/// earned only by moving bytes, so a client feeding one byte at a time cannot
/// keep the transfer open by resetting `idle`, and `idle` remains the ceiling
/// on any single gap.
#[cfg(feature = "server")]
pub struct BoundedChunkReceiver<'a> {
    inner: &'a mut dyn Receiver<Segment>,
    limits: Limits,
    started: Instant,
    moved: u64,
    chunk_size: u64,
    declared_size: Option<u64>,
    error: Option<TransferError>,
}

#[cfg(feature = "server")]
impl<'a> BoundedChunkReceiver<'a> {
    /// Creates a receiver over `inner` for a session bounded by `limits` whose
    /// agreed chunk size is `chunk_size`, holding the transfer to
    /// `declared_size` bytes if the client declared one. The transfer's clock
    /// starts now.
    ///
    /// A declaration is binding, unlike the advisory size hint of the BPA's
    /// send door: a transfer that ends at any other size fails.
    ///
    /// # Errors
    ///
    /// Returns `RESOURCE_EXHAUSTED` if the declaration is above
    /// [`MAX_TRANSFER_SIZE`], before any byte is read.
    pub fn new(
        inner: &'a mut dyn Receiver<Segment>,
        limits: Limits,
        chunk_size: ChunkSize,
        declared_size: Option<u64>,
    ) -> Result<Self, Status> {
        // A transfer is buffered a chunk at a time, but an offset into it must
        // still be addressable.
        const MAX: u64 = if MAX_TRANSFER_SIZE > isize::MAX as u64 {
            isize::MAX as u64
        } else {
            MAX_TRANSFER_SIZE
        };

        if let Some(declared_size) = declared_size
            && declared_size > MAX
        {
            return Err(Status::resource_exhausted(format!(
                "a declared transfer of {declared_size} bytes exceeds the limit of {MAX} bytes"
            )));
        }

        Ok(Self {
            inner,
            limits,
            started: Instant::now(),
            moved: 0,
            chunk_size: chunk_size.get() as u64,
            declared_size,
            error: None,
        })
    }

    /// Returns how this receiver ended the transfer, consuming it. Returns
    /// `None` if it did not end the transfer; the inner receiver may still
    /// have.
    pub fn into_error(self) -> Option<TransferError> {
        self.error
    }

    /// Returns when the client will have fallen behind the `Feed` bound if
    /// nothing more arrives.
    ///
    /// The deadline moves with every segment, so it is read again after each.
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

    /// Accounts for `segment` against the agreed chunk size and the declared
    /// size, and returns it.
    ///
    /// # Errors
    ///
    /// Returns `INVALID_ARGUMENT` if the segment is above the agreed chunk
    /// size, if more has now arrived than was declared, or if it is the final
    /// segment and less did.
    fn account(&mut self, segment: Segment) -> Result<Segment, Status> {
        let moved = segment_len(&segment);
        self.moved = self.moved.saturating_add(moved);
        if moved > self.chunk_size {
            return Err(Status::invalid_argument(format!(
                "a chunk of {moved} bytes exceeds the agreed {} bytes",
                self.chunk_size
            )));
        }
        if let Some(declared_size) = self.declared_size {
            if self.moved > declared_size {
                return Err(Status::invalid_argument(format!(
                    "the transfer declared {declared_size} bytes, more arrived"
                )));
            }
            if matches!(segment, Segment::Final(_)) && self.moved != declared_size {
                return Err(Status::invalid_argument(format!(
                    "the transfer declared {declared_size} bytes, only {} arrived",
                    self.moved
                )));
            }
        }
        Ok(segment)
    }
}

#[cfg(feature = "server")]
#[async_trait]
impl Receiver<Segment> for BoundedChunkReceiver<'_> {
    async fn recv(&mut self) -> Result<Segment, RecvError> {
        let deadline = self.deadline();
        let read = tokio::select! {
            biased;
            // An inner interruption is the inner receiver's to report.
            next = self.inner.recv() => Ok(next?),
            () = sleep_until(deadline) => Err(TransferError::Stalled),
        };
        read.and_then(|segment| self.account(segment).map_err(TransferError::Failed))
            .map_err(|error| {
                self.error.get_or_insert(error);
                RecvError
            })
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use core::{future::pending, num::NonZeroU64};

    use hardy_bpa::Bytes;
    use tokio::time::sleep;
    use tonic::Code;

    use super::*;

    /// Resolves once `receiver` has stalled, if nothing arrives first.
    async fn stall_of(receiver: &BoundedChunkReceiver<'_>) {
        sleep_until(receiver.deadline()).await;
    }

    /// Yields the segments it was queued with, then nothing ever again.
    struct Queued(Vec<Segment>);

    #[async_trait]
    impl Receiver<Segment> for Queued {
        async fn recv(&mut self) -> Result<Segment, RecvError> {
            if self.0.is_empty() {
                pending().await
            } else {
                Ok(self.0.remove(0))
            }
        }
    }

    fn rated() -> Limits {
        Limits {
            idle: Duration::from_secs(30),
            grace: Duration::from_secs(30),
            min_rate: NonZeroU64::new(1024),
            ..Limits::default()
        }
    }

    fn bounded(source: &mut Queued, limits: Limits) -> BoundedChunkReceiver<'_> {
        BoundedChunkReceiver::new(source, limits, ChunkSize::default(), None).unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn a_drip_feeder_is_stalled_once_its_grace_is_spent() {
        let mut source = Queued(Vec::new());
        let mut receiver = bounded(&mut source, rated());

        for second in 0..60 {
            let survived = tokio::select! {
                biased;
                () = sleep(Duration::from_secs(1)) => true,
                () = stall_of(&receiver) => false,
            };
            if !survived {
                assert_eq!(second, 30, "a drip feeder must survive its grace period");
                return;
            }
            receiver.moved += 1;
        }
        panic!("one byte per second must not outlive a 1024 byte/s minimum");
    }

    #[tokio::test(start_paused = true)]
    async fn a_client_sustaining_the_minimum_rate_is_never_stalled() {
        let mut source = Queued(Vec::new());
        let mut receiver = bounded(&mut source, rated());

        for _ in 0..600 {
            let survived = tokio::select! {
                biased;
                () = sleep(Duration::from_secs(1)) => true,
                () = stall_of(&receiver) => false,
            };
            assert!(survived, "a client at the minimum rate must not be stalled");
            receiver.moved += 1024;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_disabled_rate_floor_leaves_only_the_idle_rule() {
        let mut source = Queued(Vec::new());
        let receiver = bounded(
            &mut source,
            Limits {
                min_rate: None,
                ..rated()
            },
        );
        let started = Instant::now();

        stall_of(&receiver).await;

        assert_eq!(
            started.elapsed(),
            Duration::from_secs(30),
            "a disabled rate floor must not shorten the idle bound"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_drip_feeder_survives_a_disabled_rate_floor() {
        let mut source = Queued(Vec::new());
        let mut receiver = bounded(
            &mut source,
            Limits {
                min_rate: None,
                ..rated()
            },
        );

        for _ in 0..60 {
            let survived = tokio::select! {
                biased;
                () = sleep(Duration::from_secs(1)) => true,
                () = stall_of(&receiver) => false,
            };
            assert!(
                survived,
                "one byte per second must satisfy a disabled floor"
            );
            receiver.moved += 1;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn earned_time_never_outlives_the_idle_rule() {
        let started = Instant::now();
        let mut source = Queued(Vec::new());
        let mut receiver = bounded(&mut source, rated());
        receiver.moved = 1024 * 1024 * 1024;

        stall_of(&receiver).await;

        assert_eq!(
            started.elapsed(),
            Duration::from_secs(30),
            "a burst then silence must still be caught by the idle rule"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_source_that_stops_feeding_is_stalled_and_the_stall_is_kept() {
        let mut source = Queued(vec![Segment::Next(Bytes::from_static(b"one"))]);
        let mut receiver =
            BoundedChunkReceiver::new(&mut source, rated(), ChunkSize::default(), None).unwrap();

        assert!(matches!(receiver.recv().await, Ok(Segment::Next(_))));
        assert!(receiver.recv().await.is_err(), "a silent source must stall");

        assert!(
            matches!(receiver.into_error(), Some(TransferError::Stalled)),
            "the stall is this receiver's to report"
        );
    }

    #[tokio::test]
    async fn a_chunk_above_the_agreed_size_ends_the_transfer() {
        let mut source = Queued(vec![Segment::Next(Bytes::from(vec![0u8; 4097]))]);
        let mut receiver = BoundedChunkReceiver::new(
            &mut source,
            Limits::default(),
            ChunkSize::new(4096).unwrap(),
            None,
        )
        .unwrap();

        assert!(receiver.recv().await.is_err());

        let Some(TransferError::Failed(error)) = receiver.into_error() else {
            panic!("an oversize chunk is this receiver's to report");
        };
        assert_eq!(error.code(), Code::InvalidArgument);
        assert_eq!(
            error.message(),
            "a chunk of 4097 bytes exceeds the agreed 4096 bytes"
        );
    }

    #[tokio::test]
    async fn a_transfer_is_held_to_its_declared_size() {
        let mut source = Queued(vec![
            Segment::Next(Bytes::from_static(b"four")),
            Segment::Final(Bytes::from_static(b"four")),
        ]);
        let mut receiver = BoundedChunkReceiver::new(
            &mut source,
            Limits::default(),
            ChunkSize::default(),
            Some(5),
        )
        .unwrap();

        assert!(matches!(receiver.recv().await, Ok(Segment::Next(_))));
        assert!(receiver.recv().await.is_err());

        let Some(TransferError::Failed(error)) = receiver.into_error() else {
            panic!("a declaration mismatch is this receiver's to report");
        };
        assert_eq!(error.code(), Code::InvalidArgument);
        assert_eq!(
            error.message(),
            "the transfer declared 5 bytes, more arrived"
        );
    }

    #[tokio::test]
    async fn a_declaration_above_the_transfer_limit_is_refused_before_reading() {
        let mut source = Queued(Vec::new());
        let Err(refused) = BoundedChunkReceiver::new(
            &mut source,
            Limits::default(),
            ChunkSize::default(),
            Some(MAX_TRANSFER_SIZE + 1),
        ) else {
            panic!("a declaration above the limit must be refused");
        };
        assert_eq!(refused.code(), Code::ResourceExhausted);
    }
}
