use alloc::sync::Arc;
use alloc::vec::Vec;

use arc_swap::ArcSwap;
use hardy_async::sync::RwLock;
use hardy_bpv7::bpsec::key::{self, KeySource};

use crate::HashMap;

/// Holds named key sources and provides lock-free composite access.
///
/// Key sources are registered by name and can be added or removed at runtime.
/// Internally, a composite snapshot is rebuilt on each mutation and swapped
/// atomically via `ArcSwap`, so query-time access is lock-free.
pub struct KeyStore {
    sources: RwLock<HashMap<String, Arc<dyn KeySource>>>,
    current: ArcSwap<Composite>,
}

impl KeyStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&self, name: String, source: Arc<dyn KeySource>) -> Option<Arc<dyn KeySource>> {
        let mut sources = self.sources.write();
        let old = sources.insert(name, source);
        self.rebuild(&sources);
        old
    }

    pub fn remove(&self, name: &str) -> Option<Arc<dyn KeySource>> {
        let mut sources = self.sources.write();
        let old = sources.remove(name);
        if old.is_some() {
            self.rebuild(&sources);
        }
        old
    }

    fn rebuild(&self, sources: &HashMap<String, Arc<dyn KeySource>>) {
        self.current.store(Arc::new(Composite {
            sources: sources.values().cloned().collect(),
        }));
    }

    /// Returns a guard to the current composite snapshot.
    /// Lock-free — suitable for hot paths.
    pub fn current(&self) -> arc_swap::Guard<Arc<Composite>> {
        self.current.load()
    }
}

impl Default for KeyStore {
    fn default() -> Self {
        Self {
            sources: RwLock::new(HashMap::new()),
            current: ArcSwap::from_pointee(Composite {
                sources: Vec::new(),
            }),
        }
    }
}

/// Tries each registered key source in order, returning the first match.
pub struct Composite {
    sources: Vec<Arc<dyn KeySource>>,
}

impl KeySource for Composite {
    fn key<'a>(
        &'a self,
        source: &hardy_bpv7::eid::Eid,
        operations: &[key::Operation],
    ) -> Option<&'a key::Key> {
        self.sources.iter().find_map(|s| s.key(source, operations))
    }
}
