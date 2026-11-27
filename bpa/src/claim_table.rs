use alloc::vec::Vec;
use core::{borrow::Borrow, hash::Hash, mem::take};

use hardy_async::sync::spin::Mutex;

use crate::{HashMap, hash_map::Entry};

// Where a key stands. `Building` holds the key while its component's factory
// runs, before there is a component to hold it; only `Registered` is usable.
// Both hold the key against reuse.
enum Slot<V> {
    Building,
    Registered(V),
}

// The registrations of one kind, keyed by name or service id. A registration
// claims its key before its factory runs and files the built component
// against that claim, so a key already held is refused before anything is
// built, and the factory runs outside the lock.
//
// The lock is a spin lock: every critical section is an O(1) lookup,
// insert or swap, so the lock is never held long enough to repay an OS
// mutex's park.
pub struct ClaimTable<K, V> {
    slots: Mutex<HashMap<K, Slot<V>>>,
}

impl<K: Hash + Eq, V> ClaimTable<K, V> {
    pub fn new() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
        }
    }

    // Claims `key` for a build, or answers `false` if it is already held.
    pub fn claim(&self, key: K) -> bool {
        match self.slots.lock().entry(key) {
            Entry::Vacant(e) => {
                e.insert(Slot::Building);
                true
            }
            Entry::Occupied(_) => false,
        }
    }

    // Claims every key in `keys` for one build, all or none: answers the
    // first key already held, having claimed nothing. The keys must be
    // distinct.
    pub fn claim_all<'a>(&self, keys: &'a [K]) -> Result<(), &'a K>
    where
        K: Clone,
    {
        let mut slots = self.slots.lock();
        if let Some(held) = keys.iter().find(|key| slots.contains_key(*key)) {
            return Err(held);
        }
        for key in keys {
            slots.insert(key.clone(), Slot::Building);
        }
        Ok(())
    }

    // Releases the claim of a build that failed.
    pub fn release<Q>(&self, key: &Q)
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let mut slots = self.slots.lock();
        if matches!(slots.get(key), Some(Slot::Building)) {
            slots.remove(key);
        }
    }

    // Releases every claim of a build that failed.
    pub fn release_all(&self, keys: &[K]) {
        let mut slots = self.slots.lock();
        for key in keys {
            if matches!(slots.get(key), Some(Slot::Building)) {
                slots.remove(key);
            }
        }
    }

    // Files the built `value` against its claim. Answers `false` when the
    // claim is gone, taken by a shutdown while the factory ran: the caller
    // still owns `value`, which nothing has reached.
    pub fn file<Q>(&self, key: &Q, value: V) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        match self.slots.lock().get_mut(key) {
            Some(slot @ Slot::Building) => {
                *slot = Slot::Registered(value);
                true
            }
            _ => false,
        }
    }

    // Files one value per key against the claims `claim_all` took, all or
    // none: answers `false`, filing nothing, if a shutdown took any of them
    // while the factory ran. The caller then releases the rest with
    // `release_all`, as a failed single build does with `release`.
    pub fn file_all(&self, entries: Vec<(K, V)>) -> bool {
        let mut slots = self.slots.lock();
        let claimed = entries
            .iter()
            .all(|(key, _)| matches!(slots.get(key), Some(Slot::Building)));
        if !claimed {
            return false;
        }
        for (key, value) in entries {
            slots.insert(key, Slot::Registered(value));
        }
        true
    }

    // Whether a registration is filed under `key`; a claim answers `false`.
    pub fn contains<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        matches!(self.slots.lock().get(key), Some(Slot::Registered(_)))
    }

    // Removes the registration filed under `key`; a claim stays in place.
    pub fn remove<Q>(&self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.remove_if(key, |_| true)
    }

    // Removes the registration filed under `key` if `is_ours` accepts it,
    // so a stale holder never takes a later registration's key or claim.
    pub fn remove_if<Q>(&self, key: &Q, is_ours: impl FnOnce(&V) -> bool) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let mut slots = self.slots.lock();
        let ours = match slots.get(key) {
            Some(Slot::Registered(value)) => is_ours(value),
            _ => false,
        };
        if !ours {
            return None;
        }
        match slots.remove(key) {
            Some(Slot::Registered(value)) => Some(value),
            _ => None,
        }
    }

    // Whether `key` is claimed by a build still running, or `None` while
    // the lock is held: the registries' tests observe their factories with
    // it.
    #[cfg(test)]
    pub fn is_building<Q>(&self, key: &Q) -> Option<bool>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let slots = self.slots.try_lock()?;
        Some(matches!(slots.get(key), Some(Slot::Building)))
    }

    // Empties the table, answering the registrations. Claims are dropped
    // with it, so their builds fail to file.
    pub fn drain(&self) -> Vec<(K, V)> {
        let slots = take(&mut *self.slots.lock());
        slots
            .into_iter()
            .filter_map(|(key, slot)| match slot {
                Slot::Registered(value) => Some((key, value)),
                Slot::Building => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_key_cannot_be_claimed() {
        let table = ClaimTable::<&str, u32>::new();
        assert!(table.claim("a"));
        assert!(!table.claim("a"), "a claim holds the key");
        assert!(table.file("a", 1));
        assert!(!table.claim("a"), "a registration holds the key");
    }

    #[test]
    fn claim_all_claims_nothing_when_one_key_is_held() {
        let table = ClaimTable::<&str, u32>::new();
        assert!(table.claim("b"));
        assert_eq!(table.claim_all(&["a", "b", "c"]), Err(&"b"));
        assert!(table.claim("a"), "the set claimed nothing");
        assert!(table.claim("c"), "the set claimed nothing");
    }

    #[test]
    fn file_all_files_nothing_when_one_claim_was_drained() {
        let table = ClaimTable::<&str, u32>::new();
        assert_eq!(table.claim_all(&["a", "b"]), Ok(()));
        // A shutdown drains the table while the factory runs.
        assert!(table.drain().is_empty());
        assert!(!table.file_all(vec![("a", 1), ("b", 2)]));
        assert!(!table.contains("a"), "a was not filed");
        table.release_all(&["a", "b"]);
        assert!(table.claim("a"), "release_all freed a");
    }

    #[test]
    fn a_released_claim_frees_the_key() {
        let table = ClaimTable::<&str, u32>::new();
        assert!(table.claim("a"));
        table.release("a");
        assert!(table.claim("a"));
    }

    #[test]
    fn release_never_removes_a_registration() {
        let table = ClaimTable::<&str, u32>::new();
        assert!(table.claim("a"));
        assert!(table.file("a", 1));
        table.release("a");
        assert!(table.contains("a"));
    }

    #[test]
    fn a_drained_claim_cannot_be_filed() {
        let table = ClaimTable::<&str, u32>::new();
        assert!(table.claim("a"));
        assert!(table.claim("b"));
        assert!(table.file("b", 2));
        assert_eq!(table.drain(), vec![("b", 2)]);
        assert!(!table.file("a", 1), "shutdown took the claim");
        assert!(!table.contains("a"));
    }

    #[test]
    fn a_claim_is_not_a_registration() {
        let table = ClaimTable::<&str, u32>::new();
        assert!(table.claim("a"));
        assert!(!table.contains("a"));
        assert_eq!(table.remove_if("a", |_| true), None);
        assert!(!table.claim("a"), "remove_if left the claim in place");
    }

    #[test]
    fn remove_if_takes_only_the_accepted_registration() {
        let table = ClaimTable::<&str, u32>::new();
        assert!(table.claim("a"));
        assert!(table.file("a", 1));
        assert_eq!(table.remove_if("a", |v| *v == 2), None);
        assert!(table.contains("a"));
        assert_eq!(table.remove_if("a", |v| *v == 1), Some(1));
        assert!(table.claim("a"), "the key is free again");
    }
}
