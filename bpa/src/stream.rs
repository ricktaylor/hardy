//! Streaming primitives shared across the BPA's trait surfaces.
//!
//! The BPA's exported traits use two complementary streaming patterns:
//!
//! - **Push side** ([`Sender<T>`]): the caller passes a sink to a callee; the
//!   callee delivers items by calling `send`. Used by storage backends to
//!   stream poll results back to the BPA.
//! - **Pull side** ([`Receiver<T>`]): the callee passes a source to a caller;
//!   the caller pulls items by calling `recv`. Used by CLAs to stream bundle
//!   segments to the BPA's ingress path.
//!
//! Both sides backpressure naturally over an `async` channel, provided the
//! underlying channel is bounded.

use hardy_async::async_trait;

// ---------------------------------------------------------------------------
// Push side
// ---------------------------------------------------------------------------

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
    async fn send(&self, item: T) -> core::result::Result<(), SendError<T>>;
}

// ---------------------------------------------------------------------------
// Default channel adapters
// ---------------------------------------------------------------------------

/// Adapter that exposes a [`hardy_async::channel::Sender<T>`] as a
/// [`Sender<T>`]. Use at call sites that create a channel and hand the
/// sender into a streaming trait method.
pub(crate) struct ChannelSender<T>(pub hardy_async::channel::Sender<T>);

impl<T> ChannelSender<T> {
    /// Convenience constructor that creates a bounded
    /// [`hardy_async::channel`] and wraps the sender in a `ChannelSender`,
    /// returning it alongside the receiver.
    pub fn bounded(capacity: usize) -> (Self, hardy_async::channel::Receiver<T>) {
        let (tx, rx) = hardy_async::channel::bounded(capacity);
        (Self(tx), rx)
    }
}

#[async_trait]
impl<T: Send + 'static> Sender<T> for ChannelSender<T> {
    async fn send(&self, item: T) -> core::result::Result<(), SendError<T>> {
        self.0
            .send(item)
            .await
            .map_err(|hardy_async::channel::SendError(item)| SendError(item))
    }
}
