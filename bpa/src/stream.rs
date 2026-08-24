//! Streaming primitives shared across the BPA's trait surfaces.
//!
//! The trait surfaces use two complementary streaming patterns, each a
//! single-method trait that keeps the surface independent of any concrete
//! channel:
//!
//! - **Push side** ([`Sender<T>`]): the caller hands a sink to a callee, which
//!   delivers items by calling [`Sender::send`]. Storage backends use it to
//!   stream poll and recovery results back to the BPA — into the hybrid
//!   storage channel in production, or a `Vec`-collecting sink in the
//!   conformance tests.
//! - **Pull side** ([`Receiver<T>`]): the callee hands a source to a caller,
//!   which pulls items by calling [`Receiver::recv`]. The input doors use it
//!   to stream bundle [`Segment`]s into the BPA — CLAs through the ingress
//!   path, services through the originate path.

use hardy_async::async_trait;

/// Returned by [`Sender::send`] when the consumer has gone away and the
/// producer should stop. Wraps the rejected item so the producer can
/// recover ownership (e.g. for logging, metrics, or alternative delivery).
/// Producers should treat this as a definitive "stop streaming" signal,
/// not a transient error.
#[derive(Debug)]
pub struct SendError<T>(pub T);

impl<T> core::fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("stream consumer has gone away")
    }
}

impl<T: core::fmt::Debug> core::error::Error for SendError<T> {}

/// A consumer of streamed items, supplied by a caller to a callee so the
/// callee can push items at its own pace. Implementors typically wrap a
/// channel sender (which has interior mutability), but may equally be
/// in-memory buffers or test mocks.
///
/// `Sender<T>` is the *push* side of a stream: the producer drives
/// delivery item-by-item by calling `send`. Returns
/// `Err(SendError(item))` to signal that the consumer is gone — at
/// which point the producer should stop. The rejected item is returned
/// in the error so the producer can recover ownership.
#[async_trait]
pub trait Sender<T>: Send + Sync {
    /// Pushes one `item` to the consumer. Returns `Err(SendError(item))`,
    /// handing the item back, once the consumer has gone away — the producer
    /// should then stop.
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
/// further items will arrive. Consumers should treat this as a definitive
/// "stop pulling" signal, not a transient error.
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
/// delivery item-by-item by calling `recv`. Returns `Err(RecvError)` to
/// signal that the producer is gone and no more items will arrive — at
/// which point the consumer should stop.
///
/// **Exclusivity**: a receiver is a drain — each `recv` removes an item,
/// and the consumer's view of the stream is a total order of items. `recv`
/// therefore takes `&mut self`: exactly one consumer drains a stream, and
/// implementors can be plain state machines with no interior mutability.
/// (The [`Sender`] side is deliberately the opposite: fills commute, so
/// `send` takes `&self` and sinks may be shared.)
///
/// **Backpressure**: `recv` is async, so a slow consumer naturally
/// backpressures the producer, provided the underlying channel is bounded.
#[async_trait]
pub trait Receiver<T>: Send {
    /// Pulls the next item. Returns `Err(RecvError)` once the producer has
    /// gone away and no more items will arrive — the consumer should then stop.
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

// A racing decorator over a [`Receiver`]: fails the pull the moment `token`
// fires, even while parked awaiting the inner stream. Registration teardown
// cancels the token, giving `unregister` one broadcast point that wakes
// every in-flight stream of the registration at once — the consumer-side
// twin of a producer dropping its sender, and downstream indistinguishable
// from it (`RecvError`, surfaced by the doors as a cancelled transfer).
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
/// trait method — [`Sink::dispatch`](crate::cla::Sink::dispatch) on the CLA
/// ingress door, [`ServiceSink::send`](crate::services::ServiceSink::send)
/// on the service originate door.
///
/// `Final` marks the last segment of the bundle. The payload may be empty
/// (`Final(Bytes::new())`) to signal end-of-stream without additional data.
/// `Final` is the end-of-stream signal: consumers stop pulling once they
/// hold it. A stray `recv` after `Final` returns `Err(`[`RecvError`]`)`
/// from a channel-backed stream (the producer has dropped its sender), but
/// a whole-buffer receiver (the `Bytes` impl of [`Receiver`]) has no
/// producer to lose and keeps yielding empty `Final`s. A producer that
/// goes away *before* delivering `Final` has truncated the bundle —
/// consumers treat that as an error, never a completion.
#[derive(Debug)]
pub enum Segment {
    /// The next segment of the bundle
    Next(crate::Bytes),
    /// The last segment (may be empty)
    Final(crate::Bytes),
}

/// A complete in-memory buffer is itself a one-segment stream: `recv`
/// drains the whole buffer as a single [`Segment::Final`] — a zero-copy
/// move of the refcounted bytes, empty or not (a zero-length buffer is a
/// legitimate empty stream, not a truncation).
///
/// This receiver never returns `Err(`[`RecvError`]`)`: that error means
/// "producer gone", and an owned buffer's producer cannot go away. Once
/// drained it yields empty `Final`s; consumers honour the [`Segment`]
/// contract and stop at the first `Final`.
///
/// This is the caller-side convenience for the streamed-only trait
/// surfaces: a component that has already assembled a whole bundle or
/// payload passes `&mut data` through a streaming door, which drains it.
/// Such call sites are interim buffering, not the end state — the full
/// end-to-end streaming pipeline (see
/// `bpa/docs/streaming_pipeline_design.md`) replaces them with true
/// incremental producers over time.
#[async_trait]
impl Receiver<Segment> for crate::Bytes {
    async fn recv(&mut self) -> core::result::Result<Segment, RecvError> {
        Ok(Segment::Final(core::mem::take(self)))
    }
}

/// Errors from [`concat_stream`].
#[derive(Debug, thiserror::Error)]
pub enum ConcatError {
    /// The producer went away before delivering [`Segment::Final`]: the
    /// bundle was truncated, and the partial bytes are discarded.
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
/// bundle storage can spool a stream directly; a capacity hint (e.g. from a
/// wire schema that announces sizes up front) is a natural extension when a
/// real streaming producer lands. An empty stream (a bare
/// `Final(Bytes::new())`) yields empty bytes — the caller's parser rejects
/// those as it would any non-bundle.
pub async fn concat_stream<R: Receiver<Segment> + ?Sized>(
    stream: &mut R,
    max_size: usize,
) -> core::result::Result<crate::Bytes, ConcatError> {
    // The first segment is held as-is until a second arrives, so a
    // single-`Final` stream (the whole-buffer convenience methods) is
    // returned untouched — unconditionally zero-copy, even when the caller
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
    /// The producer went away before delivering [`Segment::Final`]: the
    /// stream was truncated, and the partial bytes are discarded.
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
/// surfaces: a component that needs a complete in-memory bundle or payload
/// (to marshal a unary wire message, write a file, or hand to a
/// whole-buffer codec) starts by buffering the stream with this helper.
/// Such implementations are interim buffering, not the end state — the full
/// end-to-end streaming pipeline (see
/// `bpa/docs/streaming_pipeline_design.md`) replaces them with true
/// segment-at-a-time consumers over time.
///
/// `total_len` is exact, not a cap: a `total_len` that is not indexable as
/// `usize` (32-bit targets) is rejected as [`BufferError::Unaddressable`]
/// before pulling a segment; a truncated stream yields [`BufferError::Cancelled`];
/// a stream exceeding `total_len` yields [`BufferError::TooLarge`]; a
/// stream completing with fewer bytes than `total_len` yields
/// [`BufferError::Underrun`] — an implementation may have sized buffers or
/// framed a transfer from the declared length, so an under-delivering
/// producer fails here at the seam.
pub async fn buffer_stream<R: Receiver<Segment> + ?Sized>(
    stream: &mut R,
    total_len: u64,
) -> core::result::Result<crate::Bytes, BufferError> {
    let Ok(max_size) = usize::try_from(total_len) else {
        return Err(BufferError::Unaddressable { total_len });
    };
    let data = concat_stream(stream, max_size).await.map_err(|e| match e {
        ConcatError::Cancelled => BufferError::Cancelled,
        ConcatError::TooLarge { size, max } => BufferError::TooLarge { size, max },
    })?;
    if data.len() != max_size {
        return Err(BufferError::Underrun {
            size: data.len(),
            expected: max_size,
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
