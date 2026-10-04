//! A retention limit shared by several receivers, and by whatever else a
//! CLA charges against it.
//!
//! Compiled only on targets with pointer-width atomic compare-and-swap,
//! which `alloc::sync::Arc` needs.

use alloc::sync::Arc;
use core::{
    fmt,
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::receiver::MaxRetainedBytes;

/// A limit on the state several [`Receiver`](crate::receiver::Receiver)s
/// retain together, in the units [`MaxRetainedBytes`] counts.
///
/// A link that runs one receiver per peer, as the Ethernet convergence
/// layer runs one per logical channel, multiplies each receiver's
/// [`MaxRetainedBytes`] by the number of peers, and an unauthenticated peer
/// can add channels at will.  Receivers joined to one budget with
/// [`Receiver::with_budget`](crate::receiver::Receiver::with_budget) are
/// bounded together: a message whose growth the budget cannot take rejects
/// its transfer as
/// [`RejectReason::BudgetFull`](crate::receiver::RejectReason::BudgetFull).
/// Each receiver's own limit still applies, and is the only fairness
/// between them: the budget serves whoever asks first.
///
/// A CLA can charge what it holds for the same link, such as streamed
/// bytes not yet read by the BPA, with [`Self::try_charge`], so that one
/// limit covers everything the link holds.
///
/// Charges are exact across threads: growth that would exceed the limit
/// fails rather than overshooting.
pub struct RetentionBudget {
    limit: MaxRetainedBytes,
    used: AtomicUsize,
}

impl RetentionBudget {
    /// A budget of `limit`, with nothing charged.
    pub fn new(limit: MaxRetainedBytes) -> Self {
        Self {
            limit,
            used: AtomicUsize::new(0),
        }
    }

    /// The configured limit.
    pub fn limit(&self) -> MaxRetainedBytes {
        self.limit
    }

    /// What is charged now, by receivers and [`Charge`]s together.
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }

    /// Charge `bytes`, or return `None` if that would exceed the limit.
    /// The charge is released when the returned [`Charge`] is dropped.
    pub fn try_charge(self: &Arc<Self>, bytes: usize) -> Option<Charge> {
        self.try_add(bytes).then(|| Charge {
            budget: Arc::clone(self),
            bytes,
        })
    }

    /// Add `bytes` to what is charged if the total stays within the limit,
    /// and report whether it did.
    pub(crate) fn try_add(&self, bytes: usize) -> bool {
        self.update(|used| used.checked_add(bytes).filter(|&n| n <= self.limit.get()))
    }

    /// Add `bytes` whatever the limit.
    pub(crate) fn add(&self, bytes: usize) {
        self.update(|used| Some(used.saturating_add(bytes)));
    }

    /// Release `bytes` charged earlier.
    pub(crate) fn release(&self, bytes: usize) {
        self.update(|used| Some(used.saturating_sub(bytes)));
    }

    /// Replace what is charged with `f` of it, unless `f` returns `None`,
    /// and report whether it was replaced.
    fn update(&self, f: impl FnMut(usize) -> Option<usize>) -> bool {
        // Relaxed: the counter guards no other memory, and the
        // read-modify-write alone keeps concurrent charges exact.
        self.used
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, f)
            .is_ok()
    }
}

impl fmt::Debug for RetentionBudget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RetentionBudget")
            .field("limit", &self.limit)
            .field("used", &self.used())
            .finish()
    }
}

/// Bytes charged against a [`RetentionBudget`], released when dropped.
///
/// Not `Clone`: each charge is released once.  A CLA keeps a charge with
/// what it accounts for, so that dropping one releases the other.
#[must_use = "dropping a charge releases it at once"]
pub struct Charge {
    budget: Arc<RetentionBudget>,
    bytes: usize,
}

impl Charge {
    /// The bytes charged.
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
    }
}

impl fmt::Debug for Charge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Charge")
            .field("bytes", &self.bytes)
            .finish()
    }
}
