//! Runtime-agnostic async primitives for Hardy DTN.
//!
//! This crate provides abstractions over async runtime primitives to enable
//! potential future support for alternative runtimes (smol, Embassy, etc.)
//! while currently using tokio.
//!
//! # Features
//!
//! - **TaskPool**: Manages cancellable tasks with graceful shutdown
//! - **BoundedTaskPool**: TaskPool with concurrency limits via semaphore
//! - **JoinHandle**: Abstracted task handle type for runtime portability
//!
//! # Example
//!
//! ```no_run
//! use hardy_async::TaskPool;
//!
//! let pool = TaskPool::new();
//! let cancel = pool.cancel_token().clone();
//!
//! pool.spawn(async move {
//!     loop {
//!         tokio::select! {
//!             _ = do_work() => {}
//!             _ = cancel.cancelled() => break,
//!         }
//!     }
//! });
//!
//! # async fn do_work() {}
//! ```

mod spawn;

pub mod bounded_task_pool;
pub mod cancellation_token;
pub mod join_handle;
pub mod notify;
pub mod task_pool;
pub mod time;

// Re-export commonly used types at crate root
pub use async_trait::async_trait;
pub use bounded_task_pool::BoundedTaskPool;
pub use cancellation_token::CancellationToken;
pub use join_handle::JoinHandle;
pub use notify::Notify;
pub use task_pool::TaskPool;
