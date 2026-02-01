//! Notify abstraction for runtime-agnostic task notification.
//!
//! This module provides [`Notify`], which wraps runtime-specific notification
//! primitives to enable future support for alternative runtimes (Embassy, smol, etc.).
//!
//! # Current Implementation
//!
//! Currently wraps `tokio::sync::Notify`. When alternative runtime support is added,
//! this will be feature-gated to provide the appropriate notification primitive.
//!
//! # Example
//!
//! ```no_run
//! use hardy_async::Notify;
//! use std::sync::Arc;
//!
//! # tokio::runtime::Runtime::new().unwrap().block_on(async {
//! let notify = Arc::new(Notify::new());
//! let notify2 = notify.clone();
//!
//! // Spawn a task that waits for notification
//! tokio::spawn(async move {
//!     notify2.notified().await;
//!     println!("Notified!");
//! });
//!
//! // Wake the waiting task
//! notify.notify_one();
//! # });
//! ```

use std::future::Future;

/// A notification primitive for waking async tasks.
///
/// `Notify` provides a simple mechanism for signaling between tasks. One task
/// can wait for a notification via [`notified()`](Notify::notified), while
/// another task signals via [`notify_one()`](Notify::notify_one).
///
/// This is a wrapper that abstracts over runtime-specific notification primitives.
/// Currently uses `tokio::sync::Notify`, but will be feature-gated for alternative
/// runtime support in the future.
///
/// # Key Methods
///
/// - [`new()`](Notify::new) - Create a new notification primitive
/// - [`notify_one()`](Notify::notify_one) - Wake one waiting task
/// - [`notified()`](Notify::notified) - Returns a future that completes when notified
#[cfg(feature = "tokio")]
pub struct Notify(tokio::sync::Notify);

#[cfg(feature = "tokio")]
impl Notify {
    /// Creates a new `Notify` instance.
    ///
    /// # Example
    ///
    /// ```
    /// use hardy_async::Notify;
    ///
    /// let notify = Notify::new();
    /// ```
    pub fn new() -> Self {
        Self(tokio::sync::Notify::new())
    }

    /// Notifies one waiting task.
    ///
    /// If a task is currently waiting on [`notified()`](Notify::notified), it will
    /// be woken. If no task is waiting, the notification is stored and the next
    /// call to `notified()` will complete immediately.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hardy_async::Notify;
    ///
    /// let notify = Notify::new();
    /// notify.notify_one();
    /// ```
    pub fn notify_one(&self) {
        self.0.notify_one();
    }

    /// Returns a future that completes when this `Notify` is signaled.
    ///
    /// The returned future will complete when [`notify_one()`](Notify::notify_one)
    /// is called. If `notify_one()` was called before `notified()`, the future
    /// completes immediately.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hardy_async::Notify;
    ///
    /// # tokio::runtime::Runtime::new().unwrap().block_on(async {
    /// let notify = Notify::new();
    ///
    /// // In another task: notify.notify_one();
    ///
    /// notify.notified().await;
    /// println!("Received notification!");
    /// # });
    /// ```
    pub fn notified(&self) -> impl Future<Output = ()> + '_ {
        self.0.notified()
    }
}

#[cfg(feature = "tokio")]
impl Default for Notify {
    fn default() -> Self {
        Self::new()
    }
}
