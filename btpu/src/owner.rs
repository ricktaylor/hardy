//! The process-unique tag that ties a [`SendId`](crate::sender::SendId) or
//! [`TransferId`](crate::transfer::TransferId) to the sender or receiver
//! that issued it.

#[cfg(target_has_atomic = "64")]
use core::sync::atomic::{AtomicU64, Ordering};

#[cfg(not(target_has_atomic = "64"))]
use portable_atomic::{AtomicU64, Ordering};

/// The next tag to hand out.  Shared by senders and receivers: a tag only
/// has to differ from every other instance's, not count one kind.
static NEXT: AtomicU64 = AtomicU64::new(0);

/// Identifies one [`Sender`](crate::sender::Sender) or
/// [`Receiver`](crate::receiver::Receiver) among every one created in the
/// process.  Each instance takes the next value of a 64-bit counter, which
/// no process can exhaust, so two live instances never share a tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Owner(u64);

impl Owner {
    /// A tag no other instance in the process has.
    pub fn new() -> Self {
        // Relaxed: only the uniqueness of the values matters, not their
        // order relative to other memory operations.
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}
