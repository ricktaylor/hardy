use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use arc_swap::ArcSwap;
use hardy_bpv7::bpsec::key::Key;
use hardy_eid_patterns::EidPattern;

use super::pattern::{PatternKeySource, SecurityRole};

/// Holds a `PatternKeySource` and provides lock-free access.
///
/// The source can be replaced atomically via `set()`, or modified
/// incrementally via `add_key()`, `remove_key()`, `add_binding()`,
/// `remove_binding()`. Incremental modifications clone the current
/// source, apply the change, and swap atomically.
pub struct KeyStore {
    current: ArcSwap<PatternKeySource>,
}

impl KeyStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the entire key source.
    pub fn set(&self, source: Arc<PatternKeySource>) {
        self.current.store(source);
    }

    /// Returns a guard to the current key source.
    /// Lock-free: suitable for hot paths.
    pub fn current(&self) -> arc_swap::Guard<Arc<PatternKeySource>> {
        self.current.load()
    }

    /// Add or replace a key by `kid`.
    pub fn add_key(&self, kid: String, key: Key) {
        self.modify(|source| source.add_key(kid, key));
    }

    /// Remove a key by `kid`. Returns true if the key existed.
    pub fn remove_key(&self, kid: &str) -> bool {
        let mut removed = false;
        self.modify(|source| removed = source.remove_key(kid));
        removed
    }

    /// Add a binding (pattern + role + key IDs).
    pub fn add_binding(&self, pattern: EidPattern, role: SecurityRole, kids: Vec<String>) {
        self.modify(|source| source.add_binding(pattern, role, kids));
    }

    /// Remove all bindings matching the given pattern.
    pub fn remove_binding(&self, pattern: &EidPattern) -> usize {
        let mut count = 0;
        self.modify(|source| count = source.remove_binding(pattern));
        count
    }

    /// Clone the current source, apply a mutation, and swap atomically.
    fn modify(&self, f: impl FnOnce(&mut PatternKeySource)) {
        let mut source = (*self.current.load_full()).clone();
        f(&mut source);
        self.current.store(Arc::new(source));
    }
}

impl Default for KeyStore {
    fn default() -> Self {
        Self {
            current: ArcSwap::from_pointee(PatternKeySource::empty()),
        }
    }
}
