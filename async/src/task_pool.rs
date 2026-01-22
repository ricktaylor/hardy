//! Task pool for managing cancellable tasks with graceful shutdown.
//!
//! This module provides [`TaskPool`], which encapsulates the common pattern of:
//! - Spawning tasks that can be cancelled via a token
//! - Tracking active tasks
//! - Graceful shutdown with guaranteed task completion
//!
//! # Pattern
//!
//! The task pool implements a three-phase shutdown:
//! 1. **Signal**: Cancel all tasks via the cancellation token
//! 2. **Close**: Prevent new tasks from spawning
//! 3. **Wait**: Block until all tasks complete
//!
//! # Example
//!
//! ```no_run
//! use hardy_async::task_pool::TaskPool;
//!
//! struct MyService {
//!     tasks: TaskPool,
//! }
//!
//! impl MyService {
//!     fn new() -> Self {
//!         Self {
//!             tasks: TaskPool::new(),
//!         }
//!     }
//!
//!     fn start(&self) {
//!         let cancel = self.tasks.cancel_token().clone();
//!         self.tasks.spawn(async move {
//!             loop {
//!                 tokio::select! {
//!                     _ = do_work() => {}
//!                     _ = cancel.cancelled() => break,
//!                 }
//!             }
//!         });
//!     }
//!
//!     async fn shutdown(&self) {
//!         self.tasks.shutdown().await;
//!     }
//! }
//!
//! async fn do_work() {
//!     // Work here
//! }
//! ```

/// Manages a group of cancellable tasks with graceful shutdown.
///
/// `TaskPool` combines a [`tokio_util::sync::CancellationToken`] and
/// [`tokio_util::task::TaskTracker`] to provide a consistent shutdown pattern.
///
/// # Shutdown Guarantees
///
/// When [`shutdown()`](TaskPool::shutdown) is called:
/// - All tasks are signaled to cancel via the cancellation token
/// - No new tasks can be spawned
/// - The method blocks until all spawned tasks complete
/// - Tasks can finish their current operation gracefully
///
/// # Child Tokens
///
/// For hierarchical cancellation (e.g., cancelling a subtask without affecting
/// the parent pool), use [`child_token()`](TaskPool::child_token).
pub struct TaskPool {
    cancel_token: tokio_util::sync::CancellationToken,
    task_tracker: tokio_util::task::TaskTracker,
}

impl TaskPool {
    /// Creates a new task pool.
    pub fn new() -> Self {
        Self {
            cancel_token: tokio_util::sync::CancellationToken::new(),
            task_tracker: tokio_util::task::TaskTracker::new(),
        }
    }

    /// Returns a reference to the cancellation token.
    ///
    /// Use this to check cancellation status or pass to tasks that need
    /// to listen for shutdown signals.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hardy_async::task_pool::TaskPool;
    /// let pool = TaskPool::new();
    /// let cancel = pool.cancel_token().clone();
    ///
    /// pool.spawn(async move {
    ///     loop {
    ///         tokio::select! {
    ///             _ = do_work() => {}
    ///             _ = cancel.cancelled() => break,
    ///         }
    ///     }
    /// });
    ///
    /// # async fn do_work() {}
    /// ```
    pub fn cancel_token(&self) -> &tokio_util::sync::CancellationToken {
        &self.cancel_token
    }

    /// Creates a child cancellation token for hierarchical cancellation.
    ///
    /// Child tokens can be cancelled independently without affecting the parent
    /// pool. However, when the parent pool is cancelled, all child tokens are
    /// also cancelled.
    ///
    /// This is useful for spawning subtasks that may need to be cancelled
    /// independently while keeping the parent pool running.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hardy_async::task_pool::TaskPool;
    /// let pool = TaskPool::new();
    /// let child = pool.child_token();
    ///
    /// // Cancel just this subtask without affecting the parent pool
    /// child.cancel();
    /// ```
    pub fn child_token(&self) -> tokio_util::sync::CancellationToken {
        self.cancel_token.child_token()
    }

    /// Spawns a task tracked by this pool.
    ///
    /// The task will be tracked until it completes. The returned [`JoinHandle`]
    /// can be used to await the task's result or check if it has finished.
    ///
    /// # Panics
    ///
    /// Panics if called after [`shutdown()`](TaskPool::shutdown) has been called,
    /// as the tracker will be closed.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hardy_async::task_pool::TaskPool;
    /// let pool = TaskPool::new();
    /// let handle = pool.spawn(async {
    ///     // Do work
    ///     42
    /// });
    ///
    /// # tokio::runtime::Runtime::new().unwrap().block_on(async {
    /// let result = handle.await.unwrap();
    /// assert_eq!(result, 42);
    /// # });
    /// ```
    pub fn spawn<F>(&self, task: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.task_tracker.spawn(task)
    }

    /// Initiates graceful shutdown and waits for all tasks to complete.
    ///
    /// This method implements the three-phase shutdown pattern:
    /// 1. Cancels all tasks via the cancellation token
    /// 2. Closes the tracker to prevent new tasks from being spawned
    /// 3. Waits for all currently running tasks to complete
    ///
    /// Tasks are expected to check the cancellation token and exit gracefully.
    /// This method will block until all tasks have finished.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use hardy_async::task_pool::TaskPool;
    /// # tokio::runtime::Runtime::new().unwrap().block_on(async {
    /// let pool = TaskPool::new();
    ///
    /// let cancel = pool.cancel_token().clone();
    /// pool.spawn(async move {
    ///     loop {
    ///         tokio::select! {
    ///             _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
    ///             _ = cancel.cancelled() => {
    ///                 // Cleanup
    ///                 break;
    ///             }
    ///         }
    ///     }
    /// });
    ///
    /// // Later...
    /// pool.shutdown().await;  // Blocks until task completes
    /// # });
    /// ```
    pub async fn shutdown(&self) {
        self.cancel_token.cancel();
        self.task_tracker.close();
        self.task_tracker.wait().await;
    }

    /// Checks if shutdown has been requested.
    ///
    /// Returns `true` if [`shutdown()`](TaskPool::shutdown) has been called
    /// or if the cancellation token has been cancelled manually.
    pub fn is_cancelled(&self) -> bool {
        self.cancel_token.is_cancelled()
    }
}

impl Default for TaskPool {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_task_pool_spawn_and_shutdown() {
        let pool = TaskPool::new();
        let cancel = pool.cancel_token().clone();

        let mut count = 0;
        pool.spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {
                        count += 1;
                    }
                    _ = cancel.cancelled() => break,
                }
            }
            count
        });

        pool.shutdown().await;
        assert!(pool.is_cancelled());
    }

    #[tokio::test]
    async fn test_child_token_independent_cancellation() {
        let pool = TaskPool::new();
        let child = pool.child_token();

        // Cancel child without affecting parent
        child.cancel();

        assert!(child.is_cancelled());
        assert!(!pool.is_cancelled());
    }

    #[tokio::test]
    async fn test_parent_cancels_child() {
        let pool = TaskPool::new();
        let child = pool.child_token();

        // Cancel parent
        pool.shutdown().await;

        // Child is also cancelled
        assert!(child.is_cancelled());
        assert!(pool.is_cancelled());
    }
}
