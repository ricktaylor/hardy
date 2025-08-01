use super::*;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

#[derive(Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct Config {
    #[serde(rename = "max-bundles")]
    pub max_bundles: std::num::NonZeroUsize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_bundles: std::num::NonZero::new(1_048_576).unwrap(),
        }
    }
}

struct Storage {
    entries: Mutex<lru::LruCache<hardy_bpv7::bundle::Id, Option<bundle::Bundle>>>,
}

#[async_trait]
impl storage::MetadataStorage for Storage {
    async fn get(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> storage::Result<Option<bundle::Bundle>> {
        if let Some(bundle) = self
            .entries
            .lock()
            .trace_expect("Failed to lock mutex")
            .get(bundle_id)
            .cloned()
        {
            Ok(bundle)
        } else {
            Ok(None)
        }
    }

    async fn insert(&self, bundle: &bundle::Bundle) -> storage::Result<bool> {
        let mut entries = self.entries.lock().trace_expect("Failed to lock mutex");
        if entries.contains(&bundle.bundle.id) {
            Ok(false)
        } else {
            entries.put(bundle.bundle.id.clone(), Some(bundle.clone()));
            Ok(true)
        }
    }

    async fn replace(&self, bundle: &bundle::Bundle) -> storage::Result<()> {
        self.entries
            .lock()
            .trace_expect("Failed to lock mutex")
            .put(bundle.bundle.id.clone(), Some(bundle.clone()));
        Ok(())
    }

    async fn tombstone(&self, bundle_id: &hardy_bpv7::bundle::Id) -> storage::Result<()> {
        self.entries
            .lock()
            .trace_expect("Failed to lock mutex")
            .put(bundle_id.clone(), None);
        Ok(())
    }

    async fn confirm_exists(
        &self,
        _bundle_id: &hardy_bpv7::bundle::Id,
    ) -> storage::Result<Option<metadata::BundleMetadata>> {
        Ok(None)
    }

    async fn remove_unconfirmed(&self, _tx: storage::Sender) -> storage::Result<()> {
        Ok(())
    }
}

pub fn new(config: &Config) -> Arc<dyn storage::MetadataStorage> {
    Arc::new(Storage {
        entries: Mutex::new(lru::LruCache::new(config.max_bundles)),
    })
}
