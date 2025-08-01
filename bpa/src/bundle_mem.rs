use super::*;
use rand::distr::{Alphanumeric, SampleString};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

#[derive(Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct Config {
    pub capacity: std::num::NonZeroUsize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            capacity: std::num::NonZero::new(1_048_576).unwrap(),
        }
    }
}

struct Storage {
    bundles: Mutex<lru::LruCache<String, Bytes>>,
}

#[async_trait]
impl storage::BundleStorage for Storage {
    async fn list(
        &self,
        tx: tokio::sync::mpsc::Sender<storage::ListResponse>,
    ) -> storage::Result<()> {
        for (name, _) in self
            .bundles
            .lock()
            .trace_expect("Failed to lock mutex")
            .iter()
        {
            tx.blocking_send((name.clone().into(), None))?;
        }
        Ok(())
    }

    async fn load(&self, storage_name: &str) -> storage::Result<Option<Bytes>> {
        Ok(self
            .bundles
            .lock()
            .trace_expect("Failed to lock mutex")
            .get(storage_name)
            .cloned())
    }

    async fn store(&self, data: Bytes) -> storage::Result<Arc<str>> {
        let mut rng = rand::rng();
        let mut bundles = self.bundles.lock().trace_expect("Failed to lock mutex");
        loop {
            let storage_name = Alphanumeric.sample_string(&mut rng, 64);
            if !bundles.contains(&storage_name) {
                bundles.put(storage_name.clone(), data);
                return Ok(storage_name.into());
            }
        }
    }

    async fn delete(&self, storage_name: &str) -> storage::Result<()> {
        self.bundles
            .lock()
            .trace_expect("Failed to lock mutex")
            .pop(storage_name);
        Ok(())
    }
}

pub fn new(config: &Config) -> Arc<dyn storage::BundleStorage> {
    Arc::new(Storage {
        bundles: Mutex::new(lru::LruCache::new(config.capacity)),
    })
}
