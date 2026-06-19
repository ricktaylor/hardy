//! Runtime-agnostic bounded and unbounded MPMC channel.
//!
//! Provides [`Sender`] and [`Receiver`] handles backed by a multi-producer,
//! multi-consumer queue. Channels close implicitly: once every `Sender` clone
//! has been dropped, [`Receiver::recv`] returns [`RecvError::Disconnected`]
//! after draining any buffered messages.
//!
//! For channels that need to signal close while a `Sender` clone is still
//! alive (typically because shutdown is coordinated by a different task to the
//! producers), see the [`closeable`](crate::closeable) module.
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
//! use hardy_async::channel;
//!
//! # tokio::runtime::Runtime::new().unwrap().block_on(async {
//! let (tx, rx) = channel::bounded::<u32>(8);
//!
//! tokio::spawn(async move {
//!     tx.send(42).await.unwrap();
//! });
//!
//! assert_eq!(rx.recv().await, Ok(42));
//! # });
//! ```

#[cfg(feature = "std")]
mod flume {

    /// A cloneable producer handle for a channel.
    ///
    /// Multiple `Sender` clones can share the same channel. The channel closes
    /// when every clone has been dropped, after which [`Receiver::recv`]
    /// returns [`RecvError::Disconnected`] once the buffer is empty.
    #[derive(Clone)]
    pub struct Sender<T>(flume::Sender<T>);

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
    /// delivered because all receivers have been dropped.
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
        /// Attempt to send a value into the channel. If the channel is bounded and full, or all
        /// receivers have been dropped, an error is returned. If the channel associated with this
        /// sender is unbounded, this method has the same behaviour as [`Sender::send`].
        #[inline]
        pub fn try_send(&self, msg: T) -> Result<(), TrySendError<T>> {
            self.0.try_send(msg).map_err(Into::into)
        }

        /// Send a value into the channel, returning an error if all receivers have been dropped.
        /// If the channel is bounded and is full, this method will block until space is available
        /// or all receivers have been dropped. If the channel is unbounded, this method will not
        /// block.
        #[inline]
        pub async fn send(&self, msg: T) -> Result<(), SendError<T>> {
            self.0.send_async(msg).await.map_err(Into::into)
        }

        /// Returns true if the channel is empty.
        #[inline]
        pub fn is_empty(&self) -> bool {
            self.0.is_empty()
        }

        /// Returns the number of messages in the channel
        #[inline]
        pub fn len(&self) -> usize {
            self.0.len()
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

    /// A consumer handle for a channel.
    ///
    /// Receives messages in FIFO order. The channel closes when every
    /// [`Sender`] clone has been dropped, after which [`recv`](Self::recv)
    /// returns [`RecvError::Disconnected`] once the buffer is empty.
    pub struct Receiver<T>(flume::Receiver<T>);

    impl<T> Receiver<T> {
        /// Wait for the next message in the channel.
        ///
        /// Returns [`RecvError::Disconnected`] once all senders have been
        /// dropped and the buffer is empty.
        #[inline]
        pub async fn recv(&self) -> Result<T, RecvError> {
            self.0.recv_async().await.map_err(Into::into)
        }

        /// Returns true if the channel is empty.
        #[inline]
        pub fn is_empty(&self) -> bool {
            self.0.is_empty()
        }

        /// Returns the number of messages in the channel
        #[inline]
        pub fn len(&self) -> usize {
            self.0.len()
        }
    }

    /// Create a bounded channel with the given buffer capacity.
    ///
    /// [`Sender::send`] awaits buffer space when the channel is full;
    /// [`Sender::try_send`] returns [`TrySendError::Full`] in the same
    /// condition.
    pub fn bounded<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
        let (tx, rx) = flume::bounded(capacity);
        (Sender(tx), Receiver(rx))
    }

    /// Create an unbounded channel.
    ///
    /// Senders never block on buffer space. [`Sender::try_send`] behaves
    /// identically to [`Sender::send`] aside from being synchronous.
    pub fn unbounded<T>() -> (Sender<T>, Receiver<T>) {
        let (tx, rx) = flume::unbounded();
        (Sender(tx), Receiver(rx))
    }
}

#[cfg(feature = "std")]
pub use self::flume::*;
