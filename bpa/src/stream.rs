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
//!   which pulls items by calling [`Receiver::recv`]. CLAs use it to stream
//!   bundle segments into the BPA's ingress path.

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
/// caller can pull items at its own pace. Implementors typically wrap a
/// channel receiver (which has interior mutability).
///
/// `Receiver<T>` is the *pull* side of a stream: the consumer drives
/// delivery item-by-item by calling `recv`. Returns `Err(RecvError)` to
/// signal that the producer is gone and no more items will arrive — at
/// which point the consumer should stop.
///
/// **Backpressure**: `recv` is async, so a slow consumer naturally
/// backpressures the producer, provided the underlying channel is bounded.
#[async_trait]
pub trait Receiver<T>: Send + Sync {
    /// Pulls the next item. Returns `Err(RecvError)` once the producer has
    /// gone away and no more items will arrive — the consumer should then stop.
    async fn recv(&self) -> core::result::Result<T, RecvError>;
}

/// A channel receiver is itself a stream [`Receiver`], so a call site can
/// create a channel and pass the receiver straight into a streaming trait
/// method.
#[async_trait]
impl<T: Send + 'static> Receiver<T> for hardy_async::channel::Receiver<T> {
    async fn recv(&self) -> core::result::Result<T, RecvError> {
        hardy_async::channel::Receiver::recv(self)
            .await
            .map_err(|_| RecvError)
    }
}
