use alloc::boxed::Box;
use alloc::sync::Arc;

use arc_swap::ArcSwap;
use hardy_bpv7::bpsec::key::{Key, KeySource, Operation};
use hardy_bpv7::eid::Eid;

use super::pattern::PatternKeySource;

/// Sized wrapper around any [`KeySource`], enabling use with `ArcSwap`.
pub struct KeyProvider(Box<dyn KeySource>);

impl KeyProvider {
    pub fn new(source: impl KeySource + 'static) -> Self {
        Self(Box::new(source))
    }
}

impl KeySource for KeyProvider {
    fn key<'a>(&'a self, source: &Eid, operations: &[Operation]) -> Option<&'a Key> {
        self.0.key(source, operations)
    }
}

/// Holds a single [`KeyProvider`] with lock-free access.
///
/// The provider can wrap any [`KeySource`] implementation:
/// `PatternKeySource` (default), a vault backend, etc.
/// Swapping is atomic.
pub struct KeyStore {
    provider: ArcSwap<KeyProvider>,
}

impl KeyStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the current provider.
    pub fn set(&self, provider: KeyProvider) {
        self.provider.store(Arc::new(provider));
    }

    /// Returns a guard to the current provider.
    /// Lock-free: suitable for hot paths.
    pub fn current(&self) -> arc_swap::Guard<Arc<KeyProvider>> {
        self.provider.load()
    }
}

impl Default for KeyStore {
    fn default() -> Self {
        Self {
            provider: ArcSwap::from_pointee(KeyProvider::new(PatternKeySource::empty())),
        }
    }
}
