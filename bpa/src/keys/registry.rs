// This is still work in progress
#![allow(dead_code)]

use super::*;
use hardy_async::sync::RwLock;

pub struct Registry {
    providers: RwLock<HashMap<String, Arc<dyn KeyProvider>>>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            providers: RwLock::new(HashMap::new()),
        }
    }

    pub fn add_provider(
        &self,
        name: String,
        provider: Arc<dyn KeyProvider>,
    ) -> Option<Arc<dyn KeyProvider>> {
        self.providers.write().insert(name, provider)
    }

    pub fn remove_provider(&self, name: &str) -> Option<Arc<dyn KeyProvider>> {
        self.providers.write().remove(name)
    }

    /// Returns a KeySource that aggregates keys from all registered providers.
    pub fn key_source(
        &self,
        bundle: &hardy_bpv7::bundle::Bundle,
        data: &[u8],
    ) -> Box<dyn hardy_bpv7::bpsec::key::KeySource> {
        // Collect KeySources from all providers
        let sources: Vec<_> = self.providers.read().values().cloned().collect();

        Box::new(CompositeKeySource {
            sources: sources
                .into_iter()
                .map(|p| p.key_source(bundle, data))
                .collect(),
        })
    }
}

/// A composite KeySource that aggregates multiple KeySources.
/// Returns the first key found from any of the sources.
pub struct CompositeKeySource {
    sources: Vec<Box<dyn hardy_bpv7::bpsec::key::KeySource>>,
}

impl hardy_bpv7::bpsec::key::KeySource for CompositeKeySource {
    fn key<'a>(
        &'a self,
        source: &hardy_bpv7::eid::Eid,
        operations: &[hardy_bpv7::bpsec::key::Operation],
    ) -> Option<&'a hardy_bpv7::bpsec::key::Key> {
        self.sources.iter().find_map(|s| s.key(source, operations))
    }
}
