//! Bounded task pool for managing tasks with concurrency limits.
//!
//! This module provides [`BoundedTaskPool`], which extends [`TaskPool`] with
//! a concurrency limit via an internal [`Semaphore`]. Tasks are spawned only
//! after acquiring a permit, providing backpressure when the pool is at
//! capacity. It names no runtime type: spawning goes through [`TaskPool`] and
//! the limit through [`Semaphore`], so it follows them to any backend.
//!
//! # Pattern
//!
//! The bounded task pool is useful when you want to limit the number of
//! concurrent tasks to prevent resource exhaustion. Common use cases:
//!
//! - Processing a stream of items with bounded parallelism
//! - Rate-limiting expensive operations
//! - Preventing unbounded memory growth from queued tasks
//!
//! # Example
//!
//! ```no_run
//! use core::num::NonZeroUsize;
//!
//! use hardy_async::bounded_task_pool::BoundedTaskPool;
//!
//! # tokio::runtime::Runtime::new().unwrap().block_on(async {
//! // Create a pool with at most 4 concurrent tasks
//! let pool = BoundedTaskPool::new(NonZeroUsize::new(4).unwrap());
//!
//! for i in 0..100 {
//!     // spawn() waits if 4 tasks are already running
//!     pool.spawn(async move {
//!         println!("Processing item {}", i);
//!     }).await;
//! }
//!
//! pool.shutdown().await;
//! # });
//! ```

use core::{future::Future, num::NonZeroUsize};

use crate::{
    CancellationToken, available_parallelism, join_handle::JoinHandle, semaphore::Semaphore,
    task_pool::TaskPool,
};

/// A task pool with bounded concurrency.
///
/// `BoundedTaskPool` wraps a [`TaskPool`] and adds a [`Semaphore`] to limit
/// the number of concurrent tasks. The [`spawn`](BoundedTaskPool::spawn) method
/// is async and will wait for a permit before spawning if the pool is at
/// capacity.
///
/// # Concurrency Limit
///
/// The `max_concurrent` parameter controls how many tasks can run simultaneously.
/// When this limit is reached, further calls to `spawn()` will wait until a
/// running task completes.
///
/// # Default Implementation
///
/// The [`Default`] implementation uses [`crate::available_parallelism()`]
/// to set the concurrency limit, matching the number of CPU cores (or 1 in no_std).
///
/// # Shutdown
///
/// Like [`TaskPool`], shutdown is graceful:
/// 1. The cancellation token is triggered
/// 2. No new tasks can be spawned
/// 3. All running tasks are awaited to completion
#[derive(Clone)]
pub struct BoundedTaskPool {
    inner: TaskPool,
    semaphore: Semaphore,
}

impl BoundedTaskPool {
    /// Creates a new bounded task pool with the specified concurrency limit.
    ///
    /// # Arguments
    ///
    /// * `max_concurrent` - Maximum number of tasks that can run simultaneously
    ///
    /// # Example
    ///
    /// ```no_run
    /// use hardy_async::bounded_task_pool::BoundedTaskPool;
    ///
    /// // Allow up to 8 concurrent tasks
    /// let pool = BoundedTaskPool::new(core::num::NonZeroUsize::new(8).unwrap());
    /// ```
    pub fn new(max_concurrent: NonZeroUsize) -> Self {
        Self {
            inner: TaskPool::new(),
            semaphore: Semaphore::new(max_concurrent),
        }
    }

    /// Spawns a task, waiting for a permit if at capacity.
    ///
    /// This method is async because it may need to wait for a running task
    /// to complete before the new task can be spawned. The permit is held
    /// for the duration of the task and automatically released when the
    /// task completes.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hardy_async::bounded_task_pool::BoundedTaskPool;
    /// # tokio::runtime::Runtime::new().unwrap().block_on(async {
    /// let pool = BoundedTaskPool::new(core::num::NonZeroUsize::new(2).unwrap());
    ///
    /// // These two spawn immediately
    /// pool.spawn(async { /* task 1 */ }).await;
    /// pool.spawn(async { /* task 2 */ }).await;
    ///
    /// // This waits until task 1 or 2 completes
    /// pool.spawn(async { /* task 3 */ }).await;
    /// # });
    /// ```
    pub async fn spawn<F>(&self, task: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let permit = self.semaphore.acquire_owned().await;

        self.inner.spawn(async move {
            let result = task.await;
            drop(permit);
            result
        })
    }

    /// Returns a reference to the cancellation token.
    ///
    /// Use this to check cancellation status or pass to tasks that need
    /// to listen for shutdown signals.
    #[inline]
    pub fn cancel_token(&self) -> &CancellationToken {
        self.inner.cancel_token()
    }

    /// Creates a child cancellation token for hierarchical cancellation.
    ///
    /// Child tokens can be cancelled independently without affecting the parent
    /// pool. However, when the parent pool is cancelled, all child tokens are
    /// also cancelled.
    #[inline]
    pub fn child_token(&self) -> CancellationToken {
        self.inner.cancel_token().child_token()
    }

    /// Initiates graceful shutdown and waits for all tasks to complete.
    ///
    /// This method:
    /// 1. Cancels all tasks via the cancellation token
    /// 2. Closes the tracker to prevent new tasks from being spawned
    /// 3. Waits for all currently running tasks to complete
    ///
    /// Tasks are expected to check the cancellation token and exit gracefully.
    pub async fn shutdown(&self) {
        self.inner.shutdown().await;
    }

    /// Checks if shutdown has been requested.
    ///
    /// Returns `true` if [`shutdown()`](BoundedTaskPool::shutdown) has been
    /// called or if the cancellation token has been cancelled manually.
    #[inline]
    pub fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }
}

impl Default for BoundedTaskPool {
    // Creates a bounded task pool with concurrency equal to available parallelism.
    //
    // Uses [`crate::available_parallelism()`] to determine the limit,
    // which queries the OS when the `std` feature is enabled, or returns 1 otherwise.
    fn default() -> Self {
        Self::new(available_parallelism())
    }
}

#[cfg(all(test, feature = "tokio"))]
mod tests {
    use futures::{pin_mut, poll};
    use tokio::sync::oneshot;

    use super::*;

    // A task that runs until `release` fires (or its sender drops), then
    // reports `id` on `done`.
    async fn hold(release: oneshot::Receiver<()>, done: flume::Sender<u32>, id: u32) {
        _ = release.await;
        _ = done.send(id);
    }

    // With every permit held by a running task, a further spawn waits; it
    // proceeds as soon as one of those tasks finishes, and never before.
    #[tokio::test]
    async fn spawn_waits_while_every_permit_is_held() {
        let pool = BoundedTaskPool::new(NonZeroUsize::new(2).unwrap());
        let (done_tx, done_rx) = flume::unbounded();

        let (release_a, held_a) = oneshot::channel();
        let (release_b, held_b) = oneshot::channel();
        pool.spawn(hold(held_a, done_tx.clone(), 1)).await;
        pool.spawn(hold(held_b, done_tx.clone(), 2)).await;

        let third = pool.spawn(async { 3 });
        pin_mut!(third);
        assert!(poll!(third.as_mut()).is_pending(), "both permits are held");

        _ = release_a.send(());
        assert_eq!(done_rx.recv_async().await.unwrap(), 1);
        let third = third.await;
        assert_eq!(third.await.unwrap(), 3);

        // Shutdown waits for the task still holding a permit, so free it.
        drop(release_b);
        pool.shutdown().await;
    }

    // The default pool admits as many concurrent tasks as the host has
    // parallelism, and no more.
    #[tokio::test]
    async fn the_default_limit_is_available_parallelism() {
        let pool = BoundedTaskPool::default();
        let limit = available_parallelism().get();
        let (done_tx, _done_rx) = flume::unbounded();

        let mut releases = Vec::with_capacity(limit);
        for id in 0..limit {
            let (release, held) = oneshot::channel();
            releases.push(release);
            pool.spawn(hold(held, done_tx.clone(), id as u32)).await;
        }
        let next = pool.spawn(async {});
        pin_mut!(next);
        assert!(
            poll!(next.as_mut()).is_pending(),
            "the default limit is reached"
        );

        drop(releases);
        next.await.await.unwrap();
        pool.shutdown().await;
    }

    // Shutdown cancels the pool's token and waits for every running task to
    // finish, so each cancellation-aware task has exited when it returns.
    #[tokio::test]
    async fn shutdown_waits_for_running_tasks() {
        let pool = BoundedTaskPool::new(NonZeroUsize::new(4).unwrap());
        let (done_tx, done_rx) = flume::unbounded();

        for id in 0..4 {
            let cancel = pool.cancel_token().clone();
            let done = done_tx.clone();
            pool.spawn(async move {
                cancel.cancelled().await;
                _ = done.send(id);
            })
            .await;
        }
        drop(done_tx);

        pool.shutdown().await;
        assert!(pool.is_cancelled());
        let mut finished: Vec<u32> = done_rx.drain().collect();
        finished.sort_unstable();
        assert_eq!(finished, vec![0, 1, 2, 3]);
    }
}
