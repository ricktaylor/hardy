use alloc::sync::Arc;

use arc_swap::ArcSwap;

use super::pattern::PatternKeySource;
use crate::HashMap;

/// Holds a `PatternKeySource` and provides lock-free access.
///
/// The source can be replaced at runtime via `set()`. Replacement is
/// atomic — in-flight lookups against the previous source complete
/// safely via `Arc` reference counting.
pub struct KeyStore {
    current: ArcSwap<PatternKeySource>,
}

impl KeyStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the current key source.
    pub fn set(&self, source: Arc<PatternKeySource>) {
        self.current.store(source);
    }

    /// Returns a guard to the current key source.
    /// Lock-free — suitable for hot paths.
    pub fn current(&self) -> arc_swap::Guard<Arc<PatternKeySource>> {
        self.current.load()
    }
}

impl Default for KeyStore {
    fn default() -> Self {
        Self {
            current: ArcSwap::from_pointee(PatternKeySource::new(HashMap::new(), vec![])),
        }
    }
}
