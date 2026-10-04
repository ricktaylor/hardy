//! The in-progress transfers and their retention charges.

use alloc::collections::BTreeMap;
#[cfg(target_has_atomic = "ptr")]
use alloc::sync::Arc;

use super::transfer::{InProgressTransfer, TransferKind};
#[cfg(target_has_atomic = "ptr")]
use crate::budget::RetentionBudget;
use crate::transfer::{TransferKey, TransferWindow};

/// The in-progress transfers and the sum of their charges.
///
/// The sum equals the transfers' charges by construction: every change to a
/// held transfer goes through [`Self::modify`], which re-charges it, and
/// every removal releases the charge of what it removes.  With a
/// [`RetentionBudget`], every change to the sum is passed on to it, so the
/// budget holds `retained - unbudgeted` for this receiver, and dropping the
/// receiver releases that.
#[derive(Default)]
pub struct Held {
    /// Keyed in window order, oldest first, so a window advance expires a
    /// leading run of entries and costs only what it expires.
    pub transfers: BTreeMap<TransferKey, InProgressTransfer>,
    /// The sum of [`InProgressTransfer::charge`] over `transfers`.
    pub retained: usize,
    /// Growth in `retained` the budget refused.  Non-zero only between the
    /// change that grew a transfer and that transfer's rejection, which
    /// releases at least as much.
    pub unbudgeted: usize,
    #[cfg(target_has_atomic = "ptr")]
    pub budget: Option<Arc<RetentionBudget>>,
}

impl Held {
    pub fn get(&self, id: TransferKey) -> Option<&InProgressTransfer> {
        self.transfers.get(&id)
    }

    pub fn len(&self) -> usize {
        self.transfers.len()
    }

    /// The newest transfer's id, if any transfer is held.
    pub fn newest(&self) -> Option<TransferKey> {
        self.transfers.last_key_value().map(|(&id, _)| id)
    }

    /// The transfer at `id`, opened as a new transfer of `kind` if none is
    /// held.  A new transfer holds nothing, so it is charged nothing.
    pub fn get_or_open(&mut self, id: TransferKey, kind: TransferKind) -> &InProgressTransfer {
        self.transfers
            .entry(id)
            .or_insert_with(|| InProgressTransfer::new(kind))
    }

    /// Apply `f` to the transfer at `id`, if one is held, and re-charge it.
    pub fn modify<R>(
        &mut self,
        id: TransferKey,
        f: impl FnOnce(&mut InProgressTransfer) -> R,
    ) -> Option<R> {
        let transfer = self.transfers.get_mut(&id)?;
        let before = transfer.charge();
        let result = f(transfer);
        let after = transfer.charge();
        // An unchanged charge, as for a streamed segment released as it
        // arrives, leaves a shared budget untouched.
        if after > before {
            self.grow(after - before);
        } else if after < before {
            self.shrink(before - after);
        }
        Some(result)
    }

    /// Remove the transfer at `id`, if one is held, releasing its charge.
    pub fn remove(&mut self, id: TransferKey) -> Option<InProgressTransfer> {
        let transfer = self.transfers.remove(&id)?;
        self.shrink(transfer.charge());
        Some(transfer)
    }

    /// Remove the oldest transfer if `window` has moved past it, releasing
    /// its charge, and return its id.
    pub fn pop_expired(&mut self, window: &TransferWindow) -> Option<TransferKey> {
        let entry = self
            .transfers
            .first_entry()
            .filter(|entry| window.is_expired(*entry.key()))?;
        let (id, transfer) = entry.remove_entry();
        self.shrink(transfer.charge());
        Some(id)
    }

    pub fn clear(&mut self) {
        self.transfers.clear();
        self.shrink(self.retained);
    }

    /// Add `bytes` to the sum, and to the budget if it has room.
    fn grow(&mut self, bytes: usize) {
        self.retained = self.retained.saturating_add(bytes);
        #[cfg(target_has_atomic = "ptr")]
        if let Some(budget) = &self.budget
            && !budget.try_add(bytes)
        {
            self.unbudgeted = self.unbudgeted.saturating_add(bytes);
        }
    }

    /// Take `bytes` from the sum, and from the budget what it was charged.
    fn shrink(&mut self, bytes: usize) {
        let bytes = bytes.min(self.retained);
        self.retained -= bytes;
        let absorbed = bytes.min(self.unbudgeted);
        self.unbudgeted -= absorbed;
        #[cfg(target_has_atomic = "ptr")]
        if let Some(budget) = &self.budget {
            budget.release(bytes - absorbed);
        }
    }

    /// What the budget holds for this receiver.
    #[cfg(target_has_atomic = "ptr")]
    fn budgeted(&self) -> usize {
        self.retained - self.unbudgeted
    }

    /// Charge everything held to `budget` instead of the current budget,
    /// whatever its limit.
    #[cfg(target_has_atomic = "ptr")]
    pub fn set_budget(&mut self, budget: Arc<RetentionBudget>) {
        if let Some(old) = self.budget.take() {
            old.release(self.budgeted());
        }
        budget.add(self.retained);
        self.unbudgeted = 0;
        self.budget = Some(budget);
    }
}

#[cfg(target_has_atomic = "ptr")]
impl Drop for Held {
    fn drop(&mut self) {
        if let Some(budget) = &self.budget {
            budget.release(self.budgeted());
        }
    }
}
