//! Streaming primitives shared across the BPA's trait surfaces.
//!
//! Storage backends stream their poll and recovery results back to the BPA
//! through the *push-side* [`Sender<T>`] trait: the BPA hands the backend a
//! sink, and the backend delivers items one at a time by calling
//! [`Sender::send`]. Keeping the trait independent of any concrete channel
//! lets a backend emit into whatever the caller chose — the hybrid storage
//! channel in production, or a `Vec`-collecting sink in the conformance
//! tests — without depending on a channel type.
//!
//! The pull side (a caller draining items with `recv`) has no trait of its
//! own; consumers use a concrete channel receiver directly.

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
