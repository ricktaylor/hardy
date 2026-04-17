//! Crash recovery for the BPA storage subsystem.
//!
//! Bundle data and metadata live in separate stores with no transactional
//! coupling. A crash during a cross-store operation (store, delete, or
//! filter mutation) can leave them inconsistent. This module reconciles
//! the two stores on startup.
//!
//! Recovery uses a typestate pattern to enforce the three-phase protocol
//! at compile time. Each phase consumes the previous state, preventing
//! phases from being skipped or reordered:
//!
//! ```text
//! Recovery<Idle>
//!     .mark().await        → marks all metadata as unconfirmed
//! Recovery<Marked>
//!     .reconcile().await   → walks bundle data, confirms or re-creates metadata
//! Recovery<Confirmed>
//!     .purge().await       → deletes orphaned metadata with no matching data
//! ```
//!
//! The struct borrows `Store` and `Dispatcher` — the spawning task owns the
//! Arcs, `Recovery` just references them. Zero allocation.

use crate::Arc;
use crate::dispatcher::Dispatcher;
use crate::storage::Store;
use core::marker::PhantomData;

mod confirmed;
mod idle;
mod marked;

// Typestate markers — zero-sized, enforce phase ordering at compile time.
pub(crate) struct Idle;
pub(crate) struct Marked;
pub(crate) struct Confirmed;

pub(crate) struct Recovery<'a, S = Idle> {
    pub(super) store: &'a Arc<Store>,
    pub(super) dispatcher: &'a Arc<Dispatcher>,
    _state: PhantomData<S>,
}

impl<'a, S> Recovery<'a, S> {
    fn transition<T>(self) -> Recovery<'a, T> {
        Recovery {
            store: self.store,
            dispatcher: self.dispatcher,
            _state: PhantomData,
        }
    }
}
