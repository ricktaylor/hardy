//! Streaming primitives shared across the BPA's trait surfaces.
//!
//! The trait surfaces use two complementary streaming patterns, each a
//! single-method trait that keeps the surface independent of any concrete
//! channel:
//!
//! - **Push side** ([`Sender<T>`]): the caller hands a sink to a callee,
//!   which delivers items by calling [`Sender::send`]. Storage backends use
//!   it to stream poll and recovery results back to the BPA, into the
//!   storage channel in production, or a `Vec`-collecting sink in the
//!   conformance tests.
//! - **Pull side** ([`Receiver<T>`]): the callee hands a source to a
//!   caller, which pulls items by calling [`Receiver::recv`]. It streams
//!   bundle [`Segment`]s into the BPA, from a CLA through the ingress path
//!   and from a service through the originate path.

use hardy_async::async_trait;

/// Returned by [`Sender::send`] when the consumer has gone away and the
/// producer should stop.
///
/// It wraps the rejected item so that the producer can recover ownership,
/// for logging, metrics or another delivery. It is a definitive signal to
/// stop streaming rather than a transient error, and nothing more: it
/// carries no verdict on the items already sent. A consumer may stop
/// reading precisely because it has accepted them (see
/// [`cla::Sink::dispatch`](crate::cla::Sink::dispatch)), and any verdict
/// rides the consuming call's own return value.
#[derive(Debug)]
pub struct SendError<T>(pub T);

impl<T> core::fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("stream consumer has gone away")
    }
}

impl<T: core::fmt::Debug> core::error::Error for SendError<T> {}

/// A consumer of streamed items, supplied by a caller to a callee so that
/// the callee can push items at its own pace.
///
/// Implementors usually wrap a channel sender, which has interior
/// mutability, but may equally be in-memory buffers or test mocks.
///
/// `Sender<T>` is the *push* side of a stream: the producer drives delivery
/// one item at a time by calling `send`. `Err(SendError(item))` says the
/// consumer is gone and the producer should stop, and hands the rejected
/// item back so that the producer can recover ownership.
#[async_trait]
pub trait Sender<T>: Send + Sync {
    /// Pushes one `item` to the consumer.
    ///
    /// # Errors
    ///
    /// Returns `Err(SendError(item))`, handing the item back, once the
    /// consumer has gone away. The producer should then stop.
    async fn send(&self, item: T) -> core::result::Result<(), SendError<T>>;
}

/// A channel sender is itself a stream [`Sender`], so a call site can create
/// a channel and pass the sender straight into a streaming trait method.
#[async_trait]
impl<T: Send + 'static> Sender<T> for hardy_async::channel::Sender<T> {
    async fn send(&self, item: T) -> core::result::Result<(), SendError<T>> {
        hardy_async::channel::Sender::send(self, item)
            .await
            .map_err(|hardy_async::channel::SendError(item)| SendError(item))
    }
}

/// Returned by [`Receiver::recv`] when the producer has gone away and no
/// further items arrive.
///
/// It is a definitive signal to stop pulling rather than a transient
/// error.
#[derive(Debug)]
pub struct RecvError;

impl core::fmt::Display for RecvError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("stream producer has gone away")
    }
}

impl core::error::Error for RecvError {}

/// A producer of streamed items, supplied by a callee to a caller so the
/// caller can pull items at its own pace.
///
/// `Receiver<T>` is the *pull* side of a stream: the consumer drives
/// delivery one item at a time by calling `recv`. `Err(RecvError)` says the
/// producer is gone, no more items arrive, and the consumer should stop.
///
/// A receiver is a drain: each `recv` removes an item, and the consumer's
/// view of the stream is a total order of items. `recv` therefore takes
/// `&mut self`, so exactly one consumer drains a stream and implementors
/// can be plain state machines with no interior mutability. The [`Sender`]
/// side is the opposite, fills commuting, so `send` takes `&self` and sinks
/// may be shared.
///
/// `recv` is async, so a slow consumer backpressures the producer, provided
/// the underlying channel is bounded.
#[async_trait]
pub trait Receiver<T>: Send {
    /// Pulls the next item.
    ///
    /// # Errors
    ///
    /// Returns `Err(RecvError)` once the producer has gone away and no more
    /// items arrive. The consumer should then stop.
    async fn recv(&mut self) -> core::result::Result<T, RecvError>;
}

/// A channel receiver is itself a stream [`Receiver`], so a call site can
/// create a channel and pass the receiver straight into a streaming trait
/// method.
#[async_trait]
impl<T: Send + 'static> Receiver<T> for hardy_async::channel::Receiver<T> {
    async fn recv(&mut self) -> core::result::Result<T, RecvError> {
        hardy_async::channel::Receiver::recv(self)
            .await
            .map_err(|_| RecvError)
    }
}

// A decorator over a [`Receiver`] that fails the pull the moment `token`
// fires, even while parked awaiting the inner stream. Registration teardown
// cancels the token, which gives `unregister` one point from which to wake
// every in-flight stream of the registration at once. It is the
// consumer-side counterpart of a producer dropping its sender, and
// downstream indistinguishable from it, arriving as `RecvError` and
// surfacing as a cancelled transfer.
pub(crate) struct CancellableReceiver<'a, T> {
    pub(crate) inner: &'a mut dyn Receiver<T>,
    pub(crate) token: hardy_async::CancellationToken,
}

#[async_trait]
impl<T: Send> Receiver<T> for CancellableReceiver<'_, T> {
    async fn recv(&mut self) -> core::result::Result<T, RecvError> {
        use futures::FutureExt;
        futures::select_biased! {
            _ = self.token.cancelled().fuse() => Err(RecvError),
            r = self.inner.recv().fuse() => r,
        }
    }
}

/// A segment of a bundle's encoded bytes in transit through a streaming
/// trait method, [`Sink::dispatch`](crate::cla::Sink::dispatch) at CLA
/// ingress or [`raw::Sink::send`](crate::service::raw::Sink::send) at
/// service origination.
///
/// `Final` marks the last segment of the bundle, and ends the stream:
/// consumers stop pulling once they hold it. Its payload may be empty,
/// `Final(Bytes::new())` signalling the end with no further data, and a
/// `Next` may be empty too, carrying nothing for consumers to skip. A stray
/// `recv` after `Final` returns `Err(`[`RecvError`]`)` from a
/// channel-backed stream, whose producer has dropped its sender, while a
/// whole-buffer receiver, the `Bytes` implementation of [`Receiver`], has
/// no producer to lose and keeps yielding empty `Final`s. A producer that
/// goes away *before* delivering `Final` has truncated the bundle, which
/// consumers treat as an error rather than a completion.
#[derive(Debug)]
pub enum Segment {
    /// The next segment of the bundle, possibly empty.
    Next(crate::Bytes),
    /// The last segment, possibly empty.
    Final(crate::Bytes),
}

/// A complete in-memory buffer is itself a one-segment stream: `recv`
/// drains the whole buffer as a single [`Segment::Final`], moving the
/// refcounted bytes without a copy, empty or not, a zero-length buffer
/// being an empty stream rather than a truncation.
///
/// This receiver never returns `Err(`[`RecvError`]`)`, which means the
/// producer is gone, and an owned buffer's producer cannot go away. Once
/// drained it yields empty `Final`s, and consumers honour the [`Segment`]
/// contract and stop at the first `Final`.
///
/// It is the caller-side convenience for the streamed-only trait surfaces:
/// a component that has already assembled a whole bundle or payload passes
/// `&mut data` to a streaming method, which drains it. Such call sites are
/// interim buffering rather than the end state, and the end-to-end
/// streaming pipeline (see `bpa/docs/streaming_pipeline_design.md`)
/// replaces them with incremental producers over time.
#[async_trait]
impl Receiver<Segment> for crate::Bytes {
    async fn recv(&mut self) -> core::result::Result<Segment, RecvError> {
        Ok(Segment::Final(core::mem::take(self)))
    }
}

/// Errors from [`concat_stream`].
#[derive(Debug, thiserror::Error)]
pub enum ConcatError {
    /// The producer went away before delivering [`Segment::Final`], so the
    /// bundle was truncated and the partial bytes are discarded.
    #[error("the stream ended before its final segment")]
    Cancelled,

    /// The accumulated segments exceeded the caller's limit.
    #[error("streamed bundle too large: {size} bytes exceeds the maximum of {max} bytes")]
    TooLarge { size: usize, max: usize },
}

/// Accumulates a complete bundle from a segment stream, refusing to grow
/// beyond `max_size` bytes.
///
/// This is the interim consumer both ends of a segment stream share until
/// bundle storage can spool a stream directly. A capacity hint, from a wire
/// schema that announces sizes up front for instance, is a natural
/// extension once a streaming producer lands. An empty stream, a bare
/// `Final(Bytes::new())`, yields empty bytes, which the caller's parser
/// rejects as it would any non-bundle.
pub async fn concat_stream<R: Receiver<Segment> + ?Sized>(
    stream: &mut R,
    max_size: usize,
) -> core::result::Result<crate::Bytes, ConcatError> {
    // The first segment is held as it is until a second arrives, so a
    // stream of one `Final`, as the whole-buffer convenience methods
    // produce, is returned untouched, without a copy even where the caller
    // retains a clone of the `Bytes`.
    let mut first: Option<crate::Bytes> = None;
    let mut concat: Option<crate::BytesMut> = None;
    let mut total = 0usize;
    loop {
        let (data, last) = match stream.recv().await {
            Ok(Segment::Next(data)) => (data, false),
            Ok(Segment::Final(data)) => (data, true),
            Err(_) => return Err(ConcatError::Cancelled),
        };

        total = total.saturating_add(data.len());
        if total > max_size {
            return Err(ConcatError::TooLarge {
                size: total,
                max: max_size,
            });
        }

        if let Some(current) = concat.as_mut() {
            current.extend_from_slice(&data);
        } else if let Some(head) = first.take() {
            let mut current = match head.try_into_mut() {
                Ok(head) => head,
                Err(head) => {
                    let mut current = crate::BytesMut::with_capacity(head.len() + data.len());
                    current.extend_from_slice(&head);
                    current
                }
            };
            current.extend_from_slice(&data);
            concat = Some(current);
        } else {
            first = Some(data);
        }

        if last {
            break;
        }
    }
    match (first, concat) {
        (Some(data), None) => Ok(data),
        (None, Some(buffer)) => Ok(buffer.into()),
        // The loop only breaks after storing a processed `Final`, so exactly
        // one accumulator is populated.
        _ => unreachable!("the loop exits only via a processed Final"),
    }
}

/// Errors from [`buffer_stream`].
#[derive(Debug, thiserror::Error)]
pub enum BufferError {
    /// The producer went away before delivering [`Segment::Final`], so the
    /// stream was truncated and the partial bytes are discarded.
    #[error("the stream ended before its final segment")]
    Cancelled,

    /// The accumulated segments exceeded the declared `total_len`.
    #[error("streamed data too large: {size} bytes exceeds the maximum of {max} bytes")]
    TooLarge { size: usize, max: usize },

    /// The declared `total_len` cannot be indexed as `usize` on this target.
    #[error("declared length of {total_len} bytes is unaddressable on this target")]
    Unaddressable { total_len: u64 },

    /// The stream completed with fewer bytes than its declared `total_len`.
    #[error("stream delivered {size} bytes of the {expected} declared")]
    Underrun { size: usize, expected: usize },
}

/// Buffers a segment stream into contiguous bytes, enforcing the declared
/// `total_len` exactly.
///
/// This is the implementor-side convenience for the streamed-only trait
/// surfaces: a component that needs a complete in-memory bundle or payload,
/// to marshal a unary wire message, write a file, or hand to a whole-buffer
/// codec, starts by buffering the stream with this helper. Such
/// implementations are interim buffering rather than the end state, and the
/// end-to-end streaming pipeline (see
/// `bpa/docs/streaming_pipeline_design.md`) replaces them with
/// segment-at-a-time consumers over time.
///
/// # Errors
///
/// `total_len` is exact rather than a cap. A `total_len` that is not
/// indexable as `usize`, on a 32-bit target, is rejected as
/// [`BufferError::Unaddressable`] before a segment is pulled, a truncated
/// stream yields [`BufferError::Cancelled`], a stream exceeding `total_len`
/// yields [`BufferError::TooLarge`], and a stream completing with fewer
/// bytes than `total_len` yields [`BufferError::Underrun`]: an
/// implementation may have sized buffers or framed a transfer from the
/// declared length, so an under-delivering producer fails here.
pub async fn buffer_stream<R: Receiver<Segment> + ?Sized>(
    stream: &mut R,
    total_len: u64,
) -> core::result::Result<crate::Bytes, BufferError> {
    let total_len =
        usize::try_from(total_len).map_err(|_| BufferError::Unaddressable { total_len })?;
    let data = concat_stream(stream, total_len)
        .await
        .map_err(|e| match e {
            ConcatError::Cancelled => BufferError::Cancelled,
            ConcatError::TooLarge { size, max } => BufferError::TooLarge { size, max },
        })?;
    if data.len() != total_len {
        return Err(BufferError::Underrun {
            size: data.len(),
            expected: total_len,
        });
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn feed(segments: Vec<Segment>) -> hardy_async::channel::Receiver<Segment> {
        let (tx, rx) = hardy_async::channel::bounded(segments.len().max(1));
        for segment in segments {
            hardy_async::channel::Sender::send(&tx, segment)
                .await
                .unwrap();
        }
        rx
    }

    #[tokio::test]
    async fn concat_reassembles_multi_segment_streams() {
        let mut rx = feed(vec![
            Segment::Next(crate::Bytes::from_static(b"he")),
            Segment::Next(crate::Bytes::from_static(b"ll")),
            Segment::Final(crate::Bytes::from_static(b"o")),
        ])
        .await;
        assert_eq!(
            concat_stream(&mut rx, usize::MAX).await.unwrap().as_ref(),
            b"hello"
        );
    }

    #[tokio::test]
    async fn concat_accepts_a_trailing_empty_final() {
        let mut rx = feed(vec![
            Segment::Next(crate::Bytes::from_static(b"data")),
            Segment::Final(crate::Bytes::new()),
        ])
        .await;
        assert_eq!(
            concat_stream(&mut rx, usize::MAX).await.unwrap().as_ref(),
            b"data"
        );
    }

    #[tokio::test]
    async fn concat_fails_a_truncated_stream() {
        let (tx, mut rx) = hardy_async::channel::bounded(1);
        hardy_async::channel::Sender::send(&tx, Segment::Next(crate::Bytes::from_static(b"part")))
            .await
            .unwrap();
        drop(tx); // no Final: the producer died mid-bundle
        assert!(matches!(
            concat_stream(&mut rx, usize::MAX).await,
            Err(ConcatError::Cancelled)
        ));
    }

    // A bounded(1) channel with a spawned producer: `recv` consuming is
    // what lets the producer make progress, pinning the pull-driven
    // backpressure contract.
    #[tokio::test]
    async fn concat_backpressures_a_bounded_producer() {
        let (tx, mut rx) = hardy_async::channel::bounded(1);
        let producer = tokio::spawn(async move {
            for chunk in [&b"aa"[..], &b"bb"[..]] {
                hardy_async::channel::Sender::send(&tx, Segment::Next(crate::Bytes::from(chunk)))
                    .await
                    .unwrap();
            }
            hardy_async::channel::Sender::send(&tx, Segment::Final(crate::Bytes::from(&b"cc"[..])))
                .await
                .unwrap();
        });
        assert_eq!(
            concat_stream(&mut rx, usize::MAX).await.unwrap().as_ref(),
            b"aabbcc"
        );
        producer.await.unwrap();
    }

    #[tokio::test]
    async fn concat_enforces_the_size_limit() {
        let mut rx = feed(vec![
            Segment::Next(crate::Bytes::from_static(b"0123456789")),
            Segment::Final(crate::Bytes::from_static(b"0123456789")),
        ])
        .await;
        assert!(matches!(
            concat_stream(&mut rx, 15).await,
            Err(ConcatError::TooLarge { size: 20, max: 15 })
        ));
    }

    #[tokio::test]
    async fn a_whole_buffer_yields_a_single_final() {
        let mut rx = crate::Bytes::from_static(b"data");
        assert!(matches!(rx.recv().await, Ok(Segment::Final(d)) if d.as_ref() == b"data"));
        // Drained: an owned buffer has no producer to lose, so it yields
        // empty Finals rather than RecvError.
        assert!(matches!(rx.recv().await, Ok(Segment::Final(d)) if d.is_empty()));
    }

    // An empty buffer is a legitimate empty stream, not a truncation.
    #[tokio::test]
    async fn an_empty_whole_buffer_is_an_empty_stream_not_a_truncation() {
        let mut rx = crate::Bytes::new();
        assert!(matches!(rx.recv().await, Ok(Segment::Final(d)) if d.is_empty()));
    }

    // The single-Final path through the buffering helper is zero-copy: the
    // bytes come back untouched, sharing the caller's allocation.
    #[tokio::test]
    async fn a_whole_buffer_passes_through_buffering_zero_copy() {
        let data = crate::Bytes::from_static(b"zero-copy");
        let mut rx = data.clone();
        let out = buffer_stream(&mut rx, data.len() as u64).await.unwrap();
        assert_eq!(out, data);
        assert_eq!(out.as_ptr(), data.as_ptr());
    }

    #[tokio::test]
    async fn buffer_accepts_an_exact_stream() {
        let mut rx = feed(vec![
            Segment::Next(crate::Bytes::from_static(b"he")),
            Segment::Final(crate::Bytes::from_static(b"llo")),
        ])
        .await;
        assert_eq!(buffer_stream(&mut rx, 5).await.unwrap().as_ref(), b"hello");
    }

    #[tokio::test]
    async fn buffer_rejects_an_underrun() {
        let mut rx = feed(vec![Segment::Final(crate::Bytes::from_static(b"abc"))]).await;
        assert!(matches!(
            buffer_stream(&mut rx, 5).await,
            Err(BufferError::Underrun {
                size: 3,
                expected: 5
            })
        ));
    }

    #[tokio::test]
    async fn buffer_rejects_an_overrun() {
        let mut rx = feed(vec![Segment::Final(crate::Bytes::from_static(b"abcdef"))]).await;
        assert!(matches!(
            buffer_stream(&mut rx, 5).await,
            Err(BufferError::TooLarge { size: 6, max: 5 })
        ));
    }

    #[tokio::test]
    async fn buffer_rejects_a_truncated_stream() {
        let (tx, mut rx) = hardy_async::channel::bounded(1);
        hardy_async::channel::Sender::send(&tx, Segment::Next(crate::Bytes::from_static(b"part")))
            .await
            .unwrap();
        drop(tx); // no Final: the producer died mid-stream
        assert!(matches!(
            buffer_stream(&mut rx, 10).await,
            Err(BufferError::Cancelled)
        ));
    }
}
