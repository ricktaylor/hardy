use super::*;
use rand::distr::{Alphanumeric, SampleString};
use std::sync::{Arc, Mutex};

#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct Config {
    pub capacity: std::num::NonZeroUsize,

    #[cfg_attr(feature = "serde", serde(rename = "min-bundles"))]
    pub min_bundles: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            capacity: std::num::NonZero::new(256 * 1_048_576).unwrap(),
            min_bundles: 32,
        }
    }
}

struct Inner {
    cache: lru::LruCache<String, Bytes>,
    capacity: usize,
}

struct Storage {
    inner: Mutex<Inner>,
    max_capacity: std::num::NonZeroUsize,
    min_bundles: usize,
}

#[async_trait]
impl storage::BundleStorage for Storage {
    async fn list(&self, tx: storage::Sender<storage::ListResponse>) -> storage::Result<()> {
        let snapshot = self
            .inner
            .lock()
            .trace_expect("Failed to lock mutex")
            .cache
            .iter()
            .map(|(n, _)| n.clone().into())
            .collect::<Vec<_>>();

        for name in snapshot {
            tx.send((name, None)).await?;
        }
        Ok(())
    }

    async fn load(&self, storage_name: &str) -> storage::Result<Option<Bytes>> {
        Ok(self
            .inner
            .lock()
            .trace_expect("Failed to lock mutex")
            .cache
            .peek(storage_name)
            .cloned())
    }

    async fn save(&self, data: Bytes) -> storage::Result<Arc<str>> {
        let mut rng = rand::rng();
        let mut inner = self.inner.lock().trace_expect("Failed to lock mutex");
        let storage_name = loop {
            let storage_name = Alphanumeric.sample_string(&mut rng, 64);
            if !inner.cache.contains(&storage_name) {
                break storage_name;
            }
        };

        let new_len = data.len();
        let old_len = inner
            .cache
            .put(storage_name.clone(), data)
            .map(|d| d.len())
            .unwrap_or(0);

        // Ensure we cap the total stored, but keep 32 bundles
        inner.capacity = inner
            .capacity
            .saturating_sub(old_len)
            .saturating_add(new_len);
        while inner.cache.len() > self.min_bundles && inner.capacity > self.max_capacity.into() {
            let Some((_, d)) = inner.cache.pop_lru() else {
                break;
            };
            inner.capacity = inner.capacity.saturating_sub(d.len());
        }

        Ok(storage_name.into())
    }

    async fn delete(&self, storage_name: &str) -> storage::Result<()> {
        let mut inner = self.inner.lock().trace_expect("Failed to lock mutex");
        if let Some(d) = inner.cache.pop(storage_name) {
            inner.capacity = inner.capacity.saturating_sub(d.len());
        }
        Ok(())
    }
}

pub fn new(config: &Config) -> Arc<dyn storage::BundleStorage> {
    Arc::new(Storage {
        inner: Mutex::new(Inner {
            cache: lru::LruCache::unbounded(),
            capacity: 0,
        }),
        max_capacity: config.capacity,
        min_bundles: config.min_bundles,
    })
}
