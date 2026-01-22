//! Runtime-agnostic async primitives for Hardy DTN.
//!
//! This crate provides abstractions over async runtime primitives to enable
//! potential future support for alternative runtimes (smol, Embassy, etc.)
//! while currently using tokio.
//!
//! # Features
//!
//! - **TaskPool**: Manages cancellable tasks with graceful shutdown
//!
//! # Example
//!
//! ```no_run
//! use hardy_async::task_pool::TaskPool;
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

pub mod task_pool;
