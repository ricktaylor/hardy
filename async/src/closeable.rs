//! Bounded and unbounded MPMC channel with explicit close.
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
//! # Current Implementation
//!
//! Currently wraps the `flume` crate. When alternative runtime support is
//! added, this will be feature-gated to provide the appropriate channel
//! primitive.
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

#[cfg(feature = "std")]
mod flume {

    /// A cloneable producer handle for a closeable channel.
    ///
    /// Multiple `Sender` clones can share the same channel. The channel closes
    /// either when every clone has been dropped, or when any clone calls
    /// [`close`](Self::close) — the latter signals all other clones via a
    /// shared [`CancellationToken`](crate::CancellationToken) so that
    /// subsequent sends fail and a waiting [`Receiver::recv`] returns
    /// [`RecvError::Disconnected`] once the buffer is empty.
    #[derive(Clone)]
    pub struct Sender<T> {
        sender: flume::Sender<T>,
        cancel_token: crate::CancellationToken,
    }

    /// An error that may be emitted when attempting to send a value into a channel on a sender when
    /// the channel is full or all receivers are dropped.
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    pub enum TrySendError<T> {
        /// The channel the message is sent on has a finite capacity and was full when the send was attempted.
        Full(T),
        /// All channel receivers were dropped and so the message has nobody to receive it.
        Disconnected(T),
    }

    impl<T> From<flume::TrySendError<T>> for TrySendError<T> {
        #[inline]
        fn from(value: flume::TrySendError<T>) -> Self {
            match value {
                flume::TrySendError::Full(t) => Self::Full(t),
                flume::TrySendError::Disconnected(t) => Self::Disconnected(t),
            }
        }
    }

    impl<T> core::fmt::Display for TrySendError<T> {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            match self {
                Self::Full(_) => f.write_str("sending on a full channel"),
                Self::Disconnected(_) => f.write_str("sending on a closed channel"),
            }
        }
    }

    impl<T: core::fmt::Debug> core::error::Error for TrySendError<T> {}

    /// An error returned by [`Sender::send`] when the message could not be
    /// delivered because the channel is closed (either by [`Sender::close`]
    /// or because all receivers have been dropped).
    ///
    /// Wraps the original message so the caller can recover ownership.
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    pub struct SendError<T>(pub T);

    impl<T> From<flume::SendError<T>> for SendError<T> {
        #[inline]
        fn from(value: flume::SendError<T>) -> Self {
            Self(value.0)
        }
    }

    impl<T> core::fmt::Display for SendError<T> {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("sending on a closed channel")
        }
    }

    impl<T: core::fmt::Debug> core::error::Error for SendError<T> {}

    impl<T> Sender<T> {
        /// Attempt to send a value into the channel without blocking.
        ///
        /// Returns [`TrySendError::Full`] if the channel is bounded and the
        /// buffer is full, or [`TrySendError::Disconnected`] if the channel
        /// has been closed — either because every receiver has been dropped or
        /// because any [`close`](Self::close) call has been made on this
        /// channel. For unbounded channels, [`TrySendError::Full`] is never
        /// returned.
        #[inline]
        pub fn try_send(&self, msg: T) -> Result<(), TrySendError<T>> {
            if self.cancel_token.is_cancelled() {
                return Err(TrySendError::Disconnected(msg));
            }
            self.sender.try_send(msg).map_err(Into::into)
        }

        /// Send a value into the channel, awaiting buffer space if the channel
        /// is bounded and full.
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
            self.sender.send_async(msg).await.map_err(Into::into)
        }

        /// Signal that no further messages will be sent through this handle.
        ///
        /// All clones of this `Sender` (and any future clones) will fail their
        /// `send`/`try_send` calls. Buffered messages remain in the channel and
        /// will be delivered to the `Receiver` until the buffer drains, at which
        /// point `Receiver::recv` returns `RecvError::Disconnected`.
        ///
        /// This is an advisory signal between sender and receiver, not a barrier:
        /// sends already in flight on other threads may still complete. Callers
        /// who need a strict happens-before guarantee should coordinate externally.
        #[inline]
        pub fn close(self) {
            self.cancel_token.cancel()
        }

        /// Returns true if the channel is empty.
        #[inline]
        pub fn is_empty(&self) -> bool {
            self.sender.is_empty()
        }

        /// Returns the number of messages in the channel
        #[inline]
        pub fn len(&self) -> usize {
            self.sender.len()
        }
    }

    /// An error that may be emitted when attempting to wait for a value on a receiver when all senders
    /// are dropped and there are no more messages in the channel.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum RecvError {
        /// All senders were dropped and no messages are waiting in the channel, so no further messages can be received.
        Disconnected,
    }

    impl From<flume::RecvError> for RecvError {
        #[inline]
        fn from(_: flume::RecvError) -> Self {
            Self::Disconnected
        }
    }

    impl core::fmt::Display for RecvError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("receiving on an empty and closed channel")
        }
    }

    impl core::error::Error for RecvError {}

    /// A consumer handle for a closeable channel.
    ///
    /// Receives messages in FIFO order. The channel closes when either
    /// [`Sender::close`] is called or every [`Sender`] clone is dropped; in
    /// both cases [`recv`](Self::recv) returns [`RecvError::Disconnected`]
    /// once the buffer has been drained.
    pub struct Receiver<T> {
        receiver: flume::Receiver<T>,
        cancel_token: crate::CancellationToken,
    }

    impl<T> Receiver<T> {
        /// Wait for the next message in the channel.
        ///
        /// Buffered messages are always delivered before disconnection is
        /// reported: once the buffer is empty and the channel is closed
        /// (either by [`Sender::close`] or by every `Sender` being dropped),
        /// returns [`RecvError::Disconnected`].
        #[cfg(feature = "tokio")]
        #[inline]
        pub async fn recv(&self) -> Result<T, RecvError> {
            tokio::select! {
                biased;

                r = self.receiver.recv_async() => {
                    r.map_err(Into::into)
                }
                _ = self.cancel_token.cancelled() => {
                    Err(RecvError::Disconnected)
                }
            }
        }

        /// Returns true if the channel is empty.
        #[inline]
        pub fn is_empty(&self) -> bool {
            self.receiver.is_empty()
        }

        /// Returns the number of messages in the channel
        #[inline]
        pub fn len(&self) -> usize {
            self.receiver.len()
        }
    }

    /// Create a bounded closeable channel with the given buffer capacity.
    ///
    /// [`Sender::send`] awaits buffer space when the channel is full;
    /// [`Sender::try_send`] returns [`TrySendError::Full`] in the same
    /// condition.
    pub fn bounded<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
        let cancel_token = crate::CancellationToken::new();
        let (tx, rx) = flume::bounded(capacity);

        let tx = Sender {
            sender: tx,
            cancel_token: cancel_token.clone(),
        };

        let rx = Receiver {
            receiver: rx,
            cancel_token,
        };

        (tx, rx)
    }

    #[cfg(all(test, feature = "tokio"))]
    mod tests {
        use super::*;

        #[tokio::test]
        async fn send_recv_round_trip() {
            let (tx, rx) = bounded::<i32>(4);
            assert!(tx.send(1).await.is_ok());
            assert!(tx.send(2).await.is_ok());
            assert_eq!(rx.recv().await, Ok(1));
            assert_eq!(rx.recv().await, Ok(2));
        }

        #[tokio::test]
        async fn close_rejects_subsequent_send() {
            let (tx, _rx) = bounded::<i32>(4);
            let tx2 = tx.clone();
            tx.close();
            assert!(matches!(tx2.send(1).await, Err(SendError(1))));
        }

        #[tokio::test]
        async fn close_rejects_subsequent_try_send() {
            let (tx, _rx) = bounded::<i32>(4);
            let tx2 = tx.clone();
            tx.close();
            assert!(matches!(
                tx2.try_send(1),
                Err(TrySendError::Disconnected(1))
            ));
        }

        #[tokio::test]
        async fn buffered_messages_drain_then_disconnect() {
            let (tx, rx) = bounded::<i32>(4);
            let tx2 = tx.clone();
            assert!(tx2.send(1).await.is_ok());
            assert!(tx2.send(2).await.is_ok());
            assert!(tx2.send(3).await.is_ok());
            tx.close();
            assert_eq!(rx.recv().await, Ok(1));
            assert_eq!(rx.recv().await, Ok(2));
            assert_eq!(rx.recv().await, Ok(3));
            assert_eq!(rx.recv().await, Err(RecvError::Disconnected));
        }

        #[tokio::test]
        async fn close_visible_to_clones() {
            let (tx, _rx) = bounded::<i32>(4);
            let tx2 = tx.clone();
            let tx3 = tx.clone();
            tx.close();
            assert!(tx2.send(1).await.is_err());
            assert!(tx3.try_send(1).is_err());
        }

        #[tokio::test]
        async fn close_is_idempotent_across_clones() {
            let (tx, _rx) = bounded::<i32>(4);
            let tx2 = tx.clone();
            let tx3 = tx.clone();
            tx.close();
            tx2.close();
            tx3.close();
        }

        #[tokio::test]
        async fn try_send_full_distinct_from_disconnected() {
            let (tx, _rx) = bounded::<i32>(2);
            assert!(tx.try_send(1).is_ok());
            assert!(tx.try_send(2).is_ok());
            assert!(matches!(tx.try_send(3), Err(TrySendError::Full(3))));
        }

        #[tokio::test]
        async fn dropping_receiver_disconnects_sender() {
            let (tx, rx) = bounded::<i32>(4);
            drop(rx);
            assert!(tx.send(1).await.is_err());
            assert!(matches!(tx.try_send(2), Err(TrySendError::Disconnected(2))));
        }

        #[tokio::test]
        async fn recv_disconnects_on_close_with_empty_buffer() {
            let (tx, rx) = bounded::<i32>(4);
            tx.close();
            assert_eq!(rx.recv().await, Err(RecvError::Disconnected));
        }

        #[tokio::test]
        async fn recv_disconnects_when_all_senders_dropped() {
            let (tx, rx) = bounded::<i32>(4);
            drop(tx);
            assert_eq!(rx.recv().await, Err(RecvError::Disconnected));
        }

        #[tokio::test]
        async fn len_reports_buffer_fill() {
            let (tx, rx) = bounded::<i32>(4);
            assert_eq!(tx.len(), 0);
            assert_eq!(rx.len(), 0);
            tx.try_send(1).unwrap();
            tx.try_send(2).unwrap();
            assert_eq!(tx.len(), 2);
            assert_eq!(rx.len(), 2);
            assert_eq!(rx.recv().await, Ok(1));
            assert_eq!(tx.len(), 1);
            assert_eq!(rx.len(), 1);
        }
    }
}

#[cfg(feature = "std")]
pub use flume::*;
