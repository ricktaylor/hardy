//! Semaphore abstraction for runtime-agnostic permit budgets.
//!
//! This module provides [`Semaphore`], which wraps runtime-specific counting
//! semaphores to enable future support for alternative runtimes (Embassy,
//! smol, etc.). Several tasks can share one budget, each holding a clone of
//! the semaphore: a task holds an [`OwnedSemaphorePermit`] for as long as it
//! occupies a slot, and dropping the permit hands the slot back.
//!
//! # Example
//!
//! ```no_run
//! use core::num::NonZeroUsize;
//!
//! use hardy_async::Semaphore;
//!
//! # tokio::runtime::Runtime::new().unwrap().block_on(async {
//! let budget = Semaphore::new(NonZeroUsize::new(2).unwrap());
//!
//! let permit = budget.acquire_owned().await;
//! tokio::spawn(async move {
//!     // ... work occupying one slot ...
//!     drop(permit);
//! });
//! # });
//! ```

use alloc::sync::Arc;
use core::num::NonZeroUsize;

/// A counting semaphore: a budget of permits shared between tasks.
///
/// This is a wrapper that abstracts over runtime-specific semaphores.
/// Currently uses `tokio::sync::Semaphore`, but will be feature-gated for
/// alternative runtime support in the future. Clones share one budget. The
/// semaphore is never closed, so acquiring a permit cannot fail.
#[cfg(feature = "tokio")]
#[derive(Clone)]
pub struct Semaphore(Arc<tokio::sync::Semaphore>);

/// A permit from a [`Semaphore`], released when dropped.
#[cfg(feature = "tokio")]
pub struct OwnedSemaphorePermit(#[allow(dead_code)] tokio::sync::OwnedSemaphorePermit);

#[cfg(feature = "tokio")]
impl Semaphore {
    /// Creates a semaphore with `permits` permits, clamped to the runtime's
    /// maximum.
    pub fn new(permits: NonZeroUsize) -> Self {
        Self(Arc::new(tokio::sync::Semaphore::new(
            permits.get().min(tokio::sync::Semaphore::MAX_PERMITS),
        )))
    }

    /// Waits for a permit and returns it; the permit is held until dropped,
    /// independently of this semaphore handle. Waiters are served in the
    /// order they began waiting.
    pub async fn acquire_owned(&self) -> OwnedSemaphorePermit {
        OwnedSemaphorePermit(
            self.0
                .clone()
                .acquire_owned()
                .await
                .expect("the semaphore is never closed"),
        )
    }
}

#[cfg(all(test, feature = "tokio"))]
mod tests {
    use futures::{pin_mut, poll};

    use super::*;

    // Permits are a shared budget across clones: once all are held, an
    // acquire on any clone waits until one is dropped, and a dropped permit
    // is handed back.
    #[tokio::test]
    async fn permits_are_held_until_dropped() {
        let budget = Semaphore::new(NonZeroUsize::new(2).unwrap());
        let shared = budget.clone();

        let first = budget.acquire_owned().await;
        let _second = shared.acquire_owned().await;

        let third = budget.acquire_owned();
        pin_mut!(third);
        assert!(poll!(third.as_mut()).is_pending());

        drop(first);
        assert!(poll!(third.as_mut()).is_ready());
    }
}
