use super::*;
use std::collections::HashMap;
use std::sync::RwLock;

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
        self.providers
            .write()
            .expect("Failed to acquire write lock!")
            .insert(name, provider)
    }

    pub fn remove_provider(&self, name: &str) -> Option<Arc<dyn KeyProvider>> {
        self.providers
            .write()
            .expect("Failed to acquire write lock!")
            .remove(name)
    }

    /// Returns a KeySource that aggregates keys from all registered providers.
    pub fn key_source(
        &self,
        bundle: &hardy_bpv7::bundle::Bundle,
        data: &[u8],
    ) -> Box<dyn KeySource> {
        // Collect KeySources from all providers
        let sources: Vec<_> = self
            .providers
            .read()
            .expect("Failed to acquire read lock!")
            .values()
            .cloned()
            .collect();

        Box::new(CompositeKeySource {
            sources: sources
                .into_iter()
                .map(|p| p.key_source(bundle, data))
                .collect(),
        })
    }
}

/// A composite KeySource that aggregates multiple KeySources.
pub struct CompositeKeySource {
    sources: Vec<Box<dyn KeySource>>,
}

impl KeySource for CompositeKeySource {
    fn keys<'a>(
        &'a self,
        source: &hardy_bpv7::eid::Eid,
        operations: &[hardy_bpv7::bpsec::key::Operation],
    ) -> Box<dyn Iterator<Item = &'a hardy_bpv7::bpsec::key::Key> + 'a> {
        let source = source.clone();
        let operations = operations.to_vec();
        Box::new(
            self.sources
                .iter()
                .flat_map(move |s| s.keys(&source, &operations)),
        )
    }
}
