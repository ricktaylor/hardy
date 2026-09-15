//! The receiving end of a chunked transfer.
//!
//! [`ChunkReceiver`] turns the messages of a call into the segments of a
//! transfer, for either end of the wire. [`BoundedChunkReceiver`] is one for
//! the server, holding the client to the session's bounds and keeping the
//! status that ended the transfer.

use hardy_async::CancellationToken;
// The BPA's `RecvError` is qualified: the name here is the receiver's own.
use hardy_bpa::{
    async_trait,
    stream::{self, Receiver, Segment},
};
use tonic::{Status, Streaming};
use tracing::debug;

use crate::grammar::Chunk;
#[cfg(feature = "server")]
use crate::{
    chunking::{Budget, MAX_DECLARED_TRANSFER_SIZE, MaxChunkSize, segment_len},
    grammar::Cancel,
    timeouts::{Stage, Timeouts},
};

/// What ended a transfer short of its next segment.
#[derive(Debug, thiserror::Error)]
enum RecvError<M> {
    /// The session ended.
    #[error("the session ended during the transfer")]
    SessionClosed,
    /// The stream carried a message that is not a chunk.
    #[error("the transfer carried a message that is not a chunk")]
    UnexpectedMessage(M),
    /// The stream closed before its last chunk.
    #[error("the transfer stream ended before its last chunk")]
    UnexpectedEof,
    /// The stream failed with a status.
    #[error("the transfer stream failed: {0}")]
    Status(Status),
}

/// A [`Receiver`] over the chunk messages of a call, handed to the component
/// or the BPA as the transfer's segment source.
///
/// Each `chunk` or `last_chunk` message becomes one segment. Anything else ends
/// the transfer: the stream closing before its last chunk, a status, the
/// session ending, or a message that is not a chunk. Through [`Receiver`] all
/// of them are a bare [`stream::RecvError`], which is what a component reads
/// as a truncated transfer: it must not act on the partial bytes. Ending the
/// transfer through the stream, rather than by dropping the reader's future,
/// is what the in-process BPA does too, so a component unwinds the same way on
/// either side of the wire.
pub struct ChunkReceiver<'a, M> {
    messages: &'a mut Streaming<M>,
    cancel: &'a CancellationToken,
    error: Option<RecvError<M>>,
}

impl<'a, M: Chunk> ChunkReceiver<'a, M> {
    /// Creates a receiver over `messages` for the session that `cancel` ends.
    pub fn new(messages: &'a mut Streaming<M>, cancel: &'a CancellationToken) -> Self {
        Self {
            messages,
            cancel,
            error: None,
        }
    }
}

#[cfg(feature = "server")]
impl<M: Chunk + Cancel> ChunkReceiver<'_, M> {
    /// Returns the status a `Send` or `Dispatch` call ends with when the
    /// transfer ended short of its last chunk, consuming the receiver. Returns
    /// `None` if it did not.
    pub fn into_error(self) -> Option<Status> {
        self.error.map(|error| match error {
            RecvError::SessionClosed => Status::unavailable("registration closed"),
            RecvError::UnexpectedMessage(message) if message.is_cancel() => {
                Status::cancelled("transfer cancelled")
            }
            RecvError::UnexpectedMessage(_) => {
                Status::invalid_argument("the message must be a chunk or a cancel")
            }
            RecvError::UnexpectedEof => {
                Status::aborted("request stream closed before the last chunk")
            }
            RecvError::Status(_) => Status::aborted("request stream failed"),
        })
    }
}

#[async_trait]
impl<M: Chunk + Send + 'static> Receiver<Segment> for ChunkReceiver<'_, M> {
    async fn recv(&mut self) -> Result<Segment, stream::RecvError> {
        let message = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(RecvError::SessionClosed),
            message = self.messages.message() => match message {
                Ok(Some(message)) => message.into_chunk().map_err(RecvError::UnexpectedMessage),
                Ok(None) => Err(RecvError::UnexpectedEof),
                Err(status) => Err(RecvError::Status(status)),
            },
        };
        message.map_err(|error| {
            debug!("{error}");
            self.error.get_or_insert(error);
            stream::RecvError
        })
    }
}

/// A [`ChunkReceiver`] over the request stream of a `Send` or `Dispatch` call,
/// which also holds the client feeding it to the session's bounds.
///
/// Besides whatever ends the stream, a chunk above the size the session
/// negotiated, an empty chunk before the last, the client falling behind the
/// `Feed` bound, or the bytes disagreeing with a declared size end the
/// transfer. The BPA sees only [`stream::RecvError`]; how the transfer broke
/// is kept for [`into_error`](BoundedChunkReceiver::into_error).
///
/// The `Feed` bound is the transfer's [`Budget`]: each wait for the next
/// segment is a wait on the client, and the time the BPA takes between them is
/// not. How fast a client may push chunks is nobody's bound here: the server
/// reads one only when the BPA asks for it, so HTTP/2 flow control already
/// paces the client, and any chunk size up to the agreed one is legal.
#[cfg(feature = "server")]
pub struct BoundedChunkReceiver<'a, M> {
    inner: ChunkReceiver<'a, M>,
    budget: Budget<'a>,
    moved: u64,
    max_chunk_size: u64,
    declared_size: Option<u64>,
    error: Option<Status>,
}

#[cfg(feature = "server")]
impl<'a, M: Chunk + Cancel> BoundedChunkReceiver<'a, M> {
    /// Creates a receiver over `messages` for the session `timeouts` belongs
    /// to and bounds, whose agreed chunk size is `max_chunk_size`, holding the
    /// transfer to `declared_size` bytes if the client declared one. The
    /// transfer's clock starts now.
    ///
    /// A declaration is binding, unlike the advisory size hint of the BPA's
    /// send door: a transfer that ends at any other size fails.
    ///
    /// # Errors
    ///
    /// Returns `RESOURCE_EXHAUSTED` if the declaration is above
    /// [`MAX_DECLARED_TRANSFER_SIZE`], before any byte is read.
    pub fn new(
        messages: &'a mut Streaming<M>,
        timeouts: &'a Timeouts,
        max_chunk_size: MaxChunkSize,
        declared_size: Option<u64>,
    ) -> Result<Self, Status> {
        if let Some(declared_size) = declared_size
            && declared_size > MAX_DECLARED_TRANSFER_SIZE
        {
            return Err(Status::resource_exhausted(format!(
                "a declared transfer of {declared_size} bytes exceeds the limit of {MAX_DECLARED_TRANSFER_SIZE} bytes"
            )));
        }

        Ok(Self {
            inner: ChunkReceiver::new(messages, timeouts.cancel_token()),
            budget: Budget::new(timeouts, Stage::Feed),
            moved: 0,
            max_chunk_size: max_chunk_size.get() as u64,
            declared_size,
            error: None,
        })
    }

    /// Returns what ended the transfer short of its last segment, consuming
    /// the receiver: a bound the client crossed, or else whatever ended the
    /// stream. Returns `None` if the transfer was not ended.
    pub fn into_error(self) -> Option<Status> {
        let Self { inner, error, .. } = self;
        error.or_else(|| inner.into_error())
    }

    /// Accounts for `segment` against the agreed chunk size and the declared
    /// size, and returns it.
    ///
    /// # Errors
    ///
    /// Returns `RESOURCE_EXHAUSTED` if the segment is above the agreed chunk
    /// size, a limit like the others the server announces, and
    /// `INVALID_ARGUMENT` if it is an empty segment that does not end the
    /// transfer, if more has now arrived than was declared, or if it is the
    /// final segment and less did, the stream contradicting its own
    /// declaration.
    fn account(&mut self, segment: Segment) -> Result<Segment, Status> {
        let last = matches!(segment, Segment::Final(_));
        let moved = segment_len(&segment);
        self.moved = self.moved.saturating_add(moved);
        if moved > self.max_chunk_size {
            return Err(Status::resource_exhausted(format!(
                "a chunk of {moved} bytes exceeds the agreed {} bytes",
                self.max_chunk_size
            )));
        }
        // An empty chunk that is not the last carries nothing and ends
        // nothing, so a stream of them is a transfer that never finishes and
        // never grows: the only thing the transport's own pacing cannot
        // eventually bring to an end.
        if moved == 0 && !last {
            return Err(Status::invalid_argument(
                "only the last chunk of a transfer may be empty",
            ));
        }
        if let Some(declared_size) = self.declared_size {
            if self.moved > declared_size {
                return Err(Status::invalid_argument(format!(
                    "the transfer declared {declared_size} bytes, more arrived"
                )));
            }
            if last && self.moved != declared_size {
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
impl<M: Chunk + Cancel + Send + 'static> Receiver<Segment> for BoundedChunkReceiver<'_, M> {
    async fn recv(&mut self) -> Result<Segment, stream::RecvError> {
        let read = match self.budget.spend(self.moved, self.inner.recv()).await {
            Ok(Ok(segment)) => self.account(segment),
            // What ended the stream is the inner receiver's to report.
            Ok(Err(ended)) => return Err(ended),
            Err(timed_out) => Err(timed_out),
        };
        read.map_err(|error| {
            self.error.get_or_insert(error);
            stream::RecvError
        })
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use core::{num::NonZeroU64, time::Duration};

    use hardy_bpa::Bytes;
    use http_body::Frame;
    use http_body_util::StreamBody;
    use prost::Message;
    use tokio::time::sleep;
    use tokio_stream::{self, StreamExt};
    use tonic::{Code, codec::Codec};
    use tonic_prost::ProstCodec;

    use super::*;
    use crate::{
        MAX_TRANSFER_SIZE,
        application::{SendMetadata, SendRequest, send_request},
        limits::Limits,
    };

    // Frames `message` as a request stream carries it.
    fn framed_message(message: SendRequest) -> Bytes {
        let payload = message.encode_to_vec();
        let mut frame = Vec::with_capacity(payload.len() + 5);
        frame.push(0);
        frame.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_be_bytes());
        frame.extend_from_slice(&payload);
        Bytes::from(frame)
    }

    // Frames the chunk message of `segment` as a request stream carries it.
    fn framed(segment: Segment) -> Bytes {
        framed_message(SendRequest::chunk(segment))
    }

    // A request stream that carries the already framed `frames`, one every
    // `delay`, and then stays open and silent.
    fn paced_frames(frames: Vec<Bytes>, delay: Duration) -> Streaming<SendRequest> {
        let frames = tokio_stream::iter(frames)
            .then(move |frame| async move {
                sleep(delay).await;
                Ok::<_, Status>(Frame::data(frame))
            })
            .chain(tokio_stream::pending());
        Streaming::new_request(
            ProstCodec::<SendRequest, SendRequest>::default().decoder(),
            StreamBody::new(frames),
            None,
            None,
        )
    }

    // A request stream that carries `segments`, one every `delay`, and then
    // stays open and silent.
    fn paced(segments: Vec<Segment>, delay: Duration) -> Streaming<SendRequest> {
        paced_frames(segments.into_iter().map(framed).collect(), delay)
    }

    // A request stream that carries `segments` the moment they are asked for
    // and then closes.
    fn closing(segments: Vec<Segment>) -> Streaming<SendRequest> {
        let frames = tokio_stream::iter(segments.into_iter().map(framed))
            .map(|frame| Ok::<_, Status>(Frame::data(frame)));
        Streaming::new_request(
            ProstCodec::<SendRequest, SendRequest>::default().decoder(),
            StreamBody::new(frames),
            None,
            None,
        )
    }

    // A request stream that carries `segments` the moment they are asked for.
    fn queued(segments: Vec<Segment>) -> Streaming<SendRequest> {
        paced(segments, Duration::ZERO)
    }

    // A request stream of `count` one byte segments, one every `delay`.
    fn dripping(count: usize, delay: Duration) -> Streaming<SendRequest> {
        paced(
            (0..count)
                .map(|_| Segment::Next(Bytes::from_static(b"1")))
                .collect(),
            delay,
        )
    }

    // The timeouts of a session holding its clients to a minimum rate.
    fn rated() -> Timeouts {
        Timeouts::new(
            Limits {
                idle: Duration::from_secs(30),
                grace: Duration::from_secs(30),
                min_rate: NonZeroU64::new(1024),
                ..Limits::default()
            },
            CancellationToken::new(),
        )
    }

    // The timeouts of a session with room to spare.
    fn unhurried() -> Timeouts {
        Timeouts::new(Limits::default(), CancellationToken::new())
    }

    fn bounded<'a>(
        messages: &'a mut Streaming<SendRequest>,
        timeouts: &'a Timeouts,
    ) -> BoundedChunkReceiver<'a, SendRequest> {
        BoundedChunkReceiver::new(messages, timeouts, MaxChunkSize::default(), None).unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn a_drip_feeding_client_is_stalled_once_its_grace_is_spent() {
        let timeouts = rated();
        let mut messages = dripping(60, Duration::from_secs(1));
        let mut receiver = bounded(&mut messages, &timeouts);

        let mut fed = 0;
        while receiver.recv().await.is_ok() {
            fed += 1;
        }

        assert_eq!(fed, 30, "a drip feeder must survive its grace and no more");
        let stalled = receiver
            .into_error()
            .expect("one byte per second must not outlive a 1024 byte/s minimum");
        assert_eq!(stalled.code(), Code::DeadlineExceeded);
        assert_eq!(timeouts.timed_out().await, Stage::Feed);
    }

    #[tokio::test(start_paused = true)]
    async fn a_prompt_client_does_not_pay_for_the_bpa_s_time() {
        let timeouts = rated();
        let mut messages = dripping(60, Duration::ZERO);
        let mut receiver = bounded(&mut messages, &timeouts);

        for _ in 0..60 {
            assert!(
                receiver.recv().await.is_ok(),
                "a client answering at once must never be stalled"
            );
            // The BPA takes longer over one segment than the whole budget.
            sleep(Duration::from_secs(60)).await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_client_that_stops_feeding_is_stalled_and_the_stall_is_kept() {
        let timeouts = rated();
        let mut messages = queued(vec![Segment::Next(Bytes::from_static(b"one"))]);
        let mut receiver = bounded(&mut messages, &timeouts);

        assert!(matches!(receiver.recv().await, Ok(Segment::Next(_))));
        assert!(receiver.recv().await.is_err(), "a silent client must stall");

        let stalled = receiver
            .into_error()
            .expect("the stall is this receiver's to report");
        assert_eq!(stalled.code(), Code::DeadlineExceeded);
        assert_eq!(timeouts.timed_out().await, Stage::Feed);
    }

    #[tokio::test]
    async fn a_stream_that_closes_early_is_reported_when_no_bound_was_crossed() {
        let timeouts = unhurried();
        let mut messages = closing(vec![Segment::Next(Bytes::from_static(b"one"))]);
        let mut receiver = bounded(&mut messages, &timeouts);

        assert!(matches!(receiver.recv().await, Ok(Segment::Next(_))));
        assert!(receiver.recv().await.is_err());

        let error = receiver
            .into_error()
            .expect("the stream's ending must be reported through the bounded receiver");
        assert_eq!(error.code(), Code::Aborted);
        assert_eq!(
            error.message(),
            "request stream closed before the last chunk"
        );
    }

    #[tokio::test]
    async fn a_chunk_above_the_agreed_size_ends_the_transfer() {
        let timeouts = unhurried();
        let mut messages = queued(vec![Segment::Next(Bytes::from(vec![0u8; 4097]))]);
        let mut receiver = BoundedChunkReceiver::new(
            &mut messages,
            &timeouts,
            MaxChunkSize::new(4096).unwrap(),
            None,
        )
        .unwrap();

        assert!(receiver.recv().await.is_err());

        let error = receiver
            .into_error()
            .expect("an oversize chunk is this receiver's to report");
        assert_eq!(error.code(), Code::ResourceExhausted);
        assert_eq!(
            error.message(),
            "a chunk of 4097 bytes exceeds the agreed 4096 bytes"
        );
    }

    #[tokio::test]
    async fn an_empty_chunk_before_the_last_ends_the_transfer() {
        let timeouts = unhurried();
        let mut messages = queued(vec![Segment::Next(Bytes::new())]);
        let mut receiver = bounded(&mut messages, &timeouts);

        assert!(receiver.recv().await.is_err());

        let error = receiver
            .into_error()
            .expect("an empty chunk is this receiver's to report");
        assert_eq!(error.code(), Code::InvalidArgument);
        assert_eq!(
            error.message(),
            "only the last chunk of a transfer may be empty"
        );
    }

    #[tokio::test]
    async fn a_message_that_is_neither_a_chunk_nor_a_cancel_ends_the_transfer() {
        let timeouts = unhurried();
        let metadata = SendRequest {
            request: Some(send_request::Request::Metadata(SendMetadata::default())),
        };
        let mut messages = paced_frames(vec![framed_message(metadata)], Duration::ZERO);
        let mut receiver = bounded(&mut messages, &timeouts);

        assert!(receiver.recv().await.is_err());

        let error = receiver
            .into_error()
            .expect("a stray message is this receiver's to report");
        assert_eq!(error.code(), Code::InvalidArgument);
        assert_eq!(error.message(), "the message must be a chunk or a cancel");
    }

    #[tokio::test]
    async fn an_empty_last_chunk_ends_the_transfer_normally() {
        let timeouts = unhurried();
        let mut messages = queued(vec![Segment::Final(Bytes::new())]);
        let mut receiver = bounded(&mut messages, &timeouts);

        assert!(matches!(receiver.recv().await, Ok(Segment::Final(_))));
        assert!(
            receiver.into_error().is_none(),
            "an empty last chunk ends a transfer, it does not break one"
        );
    }

    #[tokio::test]
    async fn a_transfer_is_held_to_its_declared_size() {
        let timeouts = unhurried();
        let mut messages = queued(vec![
            Segment::Next(Bytes::from_static(b"four")),
            Segment::Final(Bytes::from_static(b"four")),
        ]);
        let mut receiver =
            BoundedChunkReceiver::new(&mut messages, &timeouts, MaxChunkSize::default(), Some(5))
                .unwrap();

        assert!(matches!(receiver.recv().await, Ok(Segment::Next(_))));
        assert!(receiver.recv().await.is_err());

        let error = receiver
            .into_error()
            .expect("a declaration mismatch is this receiver's to report");
        assert_eq!(error.code(), Code::InvalidArgument);
        assert_eq!(
            error.message(),
            "the transfer declared 5 bytes, more arrived"
        );
    }

    #[tokio::test]
    async fn a_declaration_above_the_transfer_limit_is_refused_before_reading() {
        let timeouts = unhurried();
        let mut messages = queued(Vec::new());
        let Err(refused) = BoundedChunkReceiver::new(
            &mut messages,
            &timeouts,
            MaxChunkSize::default(),
            Some(MAX_TRANSFER_SIZE + 1),
        ) else {
            panic!("a declaration above the limit must be refused");
        };
        assert_eq!(refused.code(), Code::ResourceExhausted);
        assert_eq!(
            refused.message(),
            format!(
                "a declared transfer of {} bytes exceeds the limit of {MAX_DECLARED_TRANSFER_SIZE} bytes",
                MAX_TRANSFER_SIZE + 1
            )
        );
    }
}
