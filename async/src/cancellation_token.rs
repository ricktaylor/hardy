//! CancellationToken abstraction for runtime-agnostic cancellation signaling.
//!
//! This module provides a type alias for cancellation tokens, enabling future
//! support for alternative runtimes (Embassy, smol, etc.) via feature flags.
//!
//! # Current Implementation
//!
//! Currently wraps `tokio_util::sync::CancellationToken`. When alternative
//! runtime support is added, this will be feature-gated to provide the
//! appropriate cancellation primitive.
//!
//! # Example
//!
//! ```no_run
//! use hardy_async::CancellationToken;
//!
//! async fn example() {
//!     let token = CancellationToken::new();
//!     let child = token.child_token();
//!
//!     // In another task
//!     tokio::spawn(async move {
//!         child.cancelled().await;
//!         println!("Cancelled!");
//!     });
//!
//!     // Cancel all tokens
//!     token.cancel();
//! }
//! ```

/// A token for cooperative cancellation of async operations.
///
/// This is a type alias that abstracts over runtime-specific cancellation
/// primitives. Currently uses tokio_util's CancellationToken, but will be
/// feature-gated for alternative runtime support in the future.
///
/// # Key Methods
///
/// - `new()` - Create a new cancellation token
/// - `child_token()` - Create a child token that cancels when parent does
/// - `cancel()` - Signal cancellation
/// - `cancelled()` - Returns a future that completes when cancelled
/// - `is_cancelled()` - Check if cancellation has been requested
#[cfg(feature = "tokio")]
pub type CancellationToken = tokio_util::sync::CancellationToken;
