use hardy_async::sync::RwLock;
use hardy_bpv7::bpsec::key::{Key as Bpv7Key, KeySource as Bpv7KeySource, Operation};
use hardy_bpv7::eid::Eid;

use super::KeyProvider;
use crate::{Arc, HashMap};

pub struct KeyRegistry {
    providers: RwLock<HashMap<String, Arc<dyn KeyProvider>>>,
}

impl Default for KeyRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyRegistry {
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

    pub fn key_source(
        &self,
        bundle: &hardy_bpv7::bundle::Bundle,
        data: &[u8],
    ) -> Box<dyn hardy_bpv7::bpsec::key::KeySource> {
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
    sources: Vec<Box<dyn Bpv7KeySource>>,
}

impl Bpv7KeySource for CompositeKeySource {
    fn key<'a>(&'a self, source: &Eid, operations: &[Operation]) -> Option<&'a Bpv7Key> {
        self.sources.iter().find_map(|s| s.key(source, operations))
    }
}
