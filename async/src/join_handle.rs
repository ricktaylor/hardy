//! JoinHandle abstraction for runtime-agnostic task handles.
//!
//! This module provides a type alias for task join handles, enabling future
//! support for alternative runtimes (Embassy, etc.) via feature flags.
//!
//! # Current Implementation
//!
//! Currently wraps `tokio::task::JoinHandle`. When Embassy support is added,
//! this will be feature-gated to provide the appropriate handle type.
//!
//! # Example
//!
//! ```no_run
//! use hardy_async::JoinHandle;
//!
//! async fn example() {
//!     let pool = hardy_async::task_pool::TaskPool::new();
//!     let handle: JoinHandle<i32> = pool.spawn(async { 42 });
//!     let result = handle.await.unwrap();
//!     assert_eq!(result, 42);
//! }
//! ```

/// A handle to a spawned task that can be awaited for its result.
///
/// This is a type alias that abstracts over runtime-specific join handles.
/// Currently uses Tokio's JoinHandle, but will be feature-gated for Embassy
/// support in the future.
#[cfg(feature = "tokio")]
pub type JoinHandle<T> = tokio::task::JoinHandle<T>;
