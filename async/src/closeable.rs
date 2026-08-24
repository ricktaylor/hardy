//! Bounded MPMC channel with explicit close.
//!
//! Like [`channel`](crate::channel), but adds [`Sender::close`] for signalling
//! "no more messages from any sender" without requiring every `Sender` clone
//! to be dropped first. The close signal is delivered via a
//! [`CancellationToken`](crate::CancellationToken) shared between all handles,
//! so subsequent [`Sender::send`]/[`Sender::try_send`] calls fail and a
//! waiting [`Receiver::recv`] returns [`RecvError::Disconnected`] once the
//! buffer is drained.
//!
//! Use this variant when one task coordinates shutdown of a queue that has
//! multiple concurrent producers — typically a worker pool draining a request
//! queue, or a dispatcher fanning out to per-peer egress queues.
//!
//! # Relationship to [`channel`](crate::channel)
//!
//! `closeable::Sender` and `closeable::Receiver` are thin wrappers around the
//! corresponding [`channel`](crate::channel) handles plus a shared
//! [`CancellationToken`](crate::CancellationToken). The error types
//! ([`TrySendError`], [`SendError`], [`RecvError`]) are re-exported from
//! `channel` — there is one canonical definition for each, shared across both
//! modules. Backend choice (currently `flume`) is inherited transitively from
//! [`channel`](crate::channel).
//!
//! # Example
//!
//! ```no_run
//! use hardy_async::closeable;
//!
//! # tokio::runtime::Runtime::new().unwrap().block_on(async {
//! let (tx, rx) = closeable::bounded::<u32>(8);
//! let producer = tx.clone();
//!
//! tokio::spawn(async move {
//!     producer.send(1).await.unwrap();
//!     producer.send(2).await.unwrap();
//! });
//!
//! tx.close();
//! while let Ok(value) = rx.recv().await {
//!     println!("got {value}");
//! }
//! # });
//! ```

use futures::FutureExt;

use crate::{CancellationToken, channel};

// Single source of truth: error types are re-exported from `channel`.
pub use crate::channel::{RecvError, SendError, TrySendError};

/// A cloneable producer handle for a closeable channel.
///
/// Multiple `Sender` clones can share the same channel. The channel closes
/// either when every clone has been dropped, or when any clone calls
/// [`close`](Self::close) — the latter signals all other clones via a
/// shared [`CancellationToken`] so that subsequent sends fail and a
/// waiting [`Receiver::recv`] returns [`RecvError::Disconnected`] once
/// the buffer is empty.
#[derive(Clone)]
pub struct Sender<T> {
    inner: channel::Sender<T>,
    cancel_token: CancellationToken,
}

impl<T> Sender<T> {
    /// Attempt to send a value into the channel without blocking.
    ///
    /// Returns [`TrySendError::Full`] if the buffer is full, or
    /// [`TrySendError::Disconnected`] if the channel has been closed —
    /// either because every receiver has been dropped or because any
    /// [`close`](Self::close) call has been made on this channel.
    #[inline]
    pub fn try_send(&self, msg: T) -> Result<(), TrySendError<T>> {
        if self.cancel_token.is_cancelled() {
            return Err(TrySendError::Disconnected(msg));
        }
        self.inner.try_send(msg)
    }

    /// Send a value into the channel, awaiting buffer space if the buffer
    /// is full.
    ///
    /// Returns [`SendError`] if the channel has been closed before this
    /// `send` began — either because every receiver has been dropped or
    /// because any [`close`](Self::close) call has been made on this
    /// channel. A `send` already awaiting buffer space when `close` is
    /// called is not interrupted; see [`close`](Self::close) for details.
    #[inline]
    pub async fn send(&self, msg: T) -> Result<(), SendError<T>> {
        if self.cancel_token.is_cancelled() {
            return Err(SendError(msg));
        }
        self.inner.send(msg).await
    }

    /// Signal that no further messages will be sent through this handle.
    ///
    /// All clones of this `Sender` (and any future clones) will fail their
    /// `send`/`try_send` calls. Buffered messages remain in the channel and
    /// will be delivered to the `Receiver` until the buffer drains, at which
    /// point `Receiver::recv` returns `RecvError::Disconnected`.
    ///
    /// `close` is an advisory signal, not a synchronisation barrier. In
    /// particular, a [`send`](Self::send) that was already awaiting buffer
    /// space when `close` was called is **not** cancelled: it continues to
    /// wait, and completes when the receiver makes space. That message can
    /// therefore land in the buffer *after* the receiver has already
    /// observed an empty buffer and returned `RecvError::Disconnected`, in
    /// which case it will never be delivered. Callers that need every
    /// in-flight `send` to be observed before shutdown must coordinate
    /// externally (e.g. quiesce producers, then `close`, then drain).
    #[inline]
    pub fn close(&self) {
        self.cancel_token.cancel()
    }

    /// Returns true if the channel is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Returns the number of messages in the channel
    #[inline]
    pub fn len(&self) -> usize {
        self.inner.len()
    }
}

/// A consumer handle for a closeable channel.
///
/// Receives messages in FIFO order. The channel closes when either
/// [`Sender::close`] is called or every [`Sender`] clone is dropped; in
/// both cases [`recv`](Self::recv) returns [`RecvError::Disconnected`]
/// once the buffer has been drained.
pub struct Receiver<T> {
    inner: channel::Receiver<T>,
    cancel_token: CancellationToken,
}

impl<T> Receiver<T> {
    /// Wait for the next message in the channel.
    ///
    /// Messages already buffered when `recv` is polled are delivered
    /// before disconnection is reported: the receive arm has priority
    /// over the close signal, so a buffered message wins over an
    /// already-fired [`Sender::close`]. Once the buffer is observed
    /// empty *and* the channel is closed (either by `Sender::close` or
    /// by every `Sender` being dropped), returns
    /// [`RecvError::Disconnected`].
    ///
    /// A `send` that was awaiting buffer space when `close` was called
    /// may complete after this method has returned `Disconnected`; see
    /// [`Sender::close`] for the in-flight window.
    #[inline]
    pub async fn recv(&self) -> Result<T, RecvError> {
        // `channel::Receiver::recv` is an `async fn`, so its future is
        // not `Unpin`. Pin it on the stack so `select_biased!` accepts
        // it without an allocation.
        let recv_fut = self.inner.recv();
        futures::pin_mut!(recv_fut);
        futures::select_biased! {
            r = recv_fut.fuse() => r,
            _ = self.cancel_token.cancelled().fuse() => Err(RecvError::Disconnected),
        }
    }

    /// Returns true if the channel is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Returns the number of messages in the channel
    #[inline]
    pub fn len(&self) -> usize {
        self.inner.len()
    }
}

/// Create a bounded closeable channel with the given buffer capacity.
///
/// [`Sender::send`] awaits buffer space when the channel is full;
/// [`Sender::try_send`] returns [`TrySendError::Full`] in the same
/// condition.
pub fn bounded<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    let cancel_token = CancellationToken::new();
    let (tx, rx) = channel::bounded(capacity);

    let tx = Sender {
        inner: tx,
        cancel_token: cancel_token.clone(),
    };

    let rx = Receiver {
        inner: rx,
        cancel_token,
    };

    (tx, rx)
}
