//! Time utilities for runtime-agnostic async operations.
//!
//! This module provides time-related async primitives that abstract over
//! runtime-specific implementations, enabling future support for alternative
//! runtimes (Embassy, smol, etc.).
//!
//! # Example
//!
//! ```no_run
//! use hardy_async::time::sleep;
//! use time::Duration;
//!
//! # tokio::runtime::Runtime::new().unwrap().block_on(async {
//! // Sleep for 5 seconds
//! sleep(Duration::seconds(5)).await;
//!
//! // Negative durations return immediately
//! sleep(Duration::seconds(-1)).await;  // No-op
//! # });
//! ```

/// Sleeps for the specified duration.
///
/// This is a runtime-agnostic sleep that wraps the underlying runtime's
/// timer implementation.
///
/// # Behavior
///
/// - Positive durations: sleeps for the specified time
/// - Zero or negative durations: returns immediately without sleeping
/// - Durations exceeding `std::time::Duration::MAX`: sleeps for `MAX`
///
/// # Example
///
/// ```no_run
/// use hardy_async::time::sleep;
/// use time::Duration;
///
/// # tokio::runtime::Runtime::new().unwrap().block_on(async {
/// sleep(Duration::seconds(1)).await;
/// println!("1 second has passed");
/// # });
/// ```
#[cfg(feature = "tokio")]
pub async fn sleep(duration: time::Duration) {
    if !duration.is_positive() {
        return;
    }

    let std_duration: std::time::Duration = duration.try_into().unwrap_or(std::time::Duration::MAX);

    tokio::time::sleep(std_duration).await;
}
