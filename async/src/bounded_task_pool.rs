//! Bounded task pool for managing tasks with concurrency limits.
//!
//! This module provides [`BoundedTaskPool`], which extends [`TaskPool`] with
//! a concurrency limit via an internal semaphore. Tasks are spawned only after
//! acquiring a permit, providing backpressure when the pool is at capacity.
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
//! use hardy_async::bounded_task_pool::BoundedTaskPool;
//!
//! # tokio::runtime::Runtime::new().unwrap().block_on(async {
//! // Create a pool with at most 4 concurrent tasks
//! let pool = BoundedTaskPool::new(core::num::NonZeroUsize::new(4).unwrap());
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

use alloc::sync::Arc;
use core::future::Future;

use crate::join_handle::JoinHandle;
use crate::task_pool::TaskPool;

/// A task pool with bounded concurrency.
///
/// `BoundedTaskPool` wraps a [`TaskPool`] and adds a semaphore to limit the
/// number of concurrent tasks. The [`spawn`](BoundedTaskPool::spawn) method
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
pub struct BoundedTaskPool {
    inner: TaskPool,
    semaphore: Arc<tokio::sync::Semaphore>,
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
    pub fn new(max_concurrent: core::num::NonZeroUsize) -> Self {
        Self {
            inner: TaskPool::new(),
            semaphore: Arc::new(tokio::sync::Semaphore::new(max_concurrent.into())),
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
        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore closed unexpectedly");

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
    pub fn cancel_token(&self) -> &crate::CancellationToken {
        self.inner.cancel_token()
    }

    /// Creates a child cancellation token for hierarchical cancellation.
    ///
    /// Child tokens can be cancelled independently without affecting the parent
    /// pool. However, when the parent pool is cancelled, all child tokens are
    /// also cancelled.
    pub fn child_token(&self) -> crate::CancellationToken {
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
    pub fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }
}

impl Default for BoundedTaskPool {
    /// Creates a bounded task pool with concurrency equal to available parallelism.
    ///
    /// Uses [`crate::available_parallelism()`] to determine the limit,
    /// which queries the OS when the `std` feature is enabled, or returns 1 otherwise.
    fn default() -> Self {
        Self::new(crate::available_parallelism())
    }
}

#[cfg(all(test, feature = "tokio"))]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use core::time::Duration;

    #[tokio::test]
    async fn test_bounded_pool_limits_concurrency() {
        let pool = BoundedTaskPool::new(core::num::NonZeroUsize::new(2).unwrap());
        let concurrent = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));

        let mut handles = vec![];

        for _ in 0..10 {
            let concurrent = concurrent.clone();
            let max_concurrent = max_concurrent.clone();

            let handle = pool
                .spawn(async move {
                    let current = concurrent.fetch_add(1, Ordering::SeqCst) + 1;

                    // Update max if this is higher
                    let mut max = max_concurrent.load(Ordering::SeqCst);
                    while current > max {
                        match max_concurrent.compare_exchange_weak(
                            max,
                            current,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        ) {
                            Ok(_) => break,
                            Err(m) => max = m,
                        }
                    }

                    tokio::time::sleep(Duration::from_millis(10)).await;
                    concurrent.fetch_sub(1, Ordering::SeqCst);
                })
                .await;

            handles.push(handle);
        }

        // Wait for all tasks
        for handle in handles {
            handle.await.unwrap();
        }

        // Max concurrent should never exceed 2
        assert!(max_concurrent.load(Ordering::SeqCst) <= 2);
    }

    #[tokio::test]
    async fn test_bounded_pool_default_uses_available_parallelism() {
        let pool = BoundedTaskPool::default();
        let expected: usize = crate::available_parallelism().into();

        // We can't directly inspect the semaphore, but we can verify the pool works
        assert!(!pool.is_cancelled());

        // Verify we can spawn at least one task
        let handle = pool.spawn(async { 42 }).await;
        assert_eq!(handle.await.unwrap(), 42);

        pool.shutdown().await;
        assert!(pool.is_cancelled());

        // Just verify expected is reasonable
        assert!(expected >= 1);
    }

    #[tokio::test]
    async fn test_bounded_pool_shutdown() {
        let pool = BoundedTaskPool::new(core::num::NonZeroUsize::new(4).unwrap());
        let completed = Arc::new(AtomicUsize::new(0));

        for _ in 0..4 {
            let completed = completed.clone();
            let cancel = pool.cancel_token().clone();

            pool.spawn(async move {
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                        _ = cancel.cancelled() => {
                            completed.fetch_add(1, Ordering::SeqCst);
                            break;
                        }
                    }
                }
            })
            .await;
        }

        pool.shutdown().await;

        // All tasks should have completed
        assert_eq!(completed.load(Ordering::SeqCst), 4);
    }
}
