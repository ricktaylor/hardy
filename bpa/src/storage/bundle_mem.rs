use super::*;
use rand::distr::{Alphanumeric, SampleString};
use std::sync::Mutex;

#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct Config {
    pub capacity: core::num::NonZeroUsize,

    #[cfg_attr(feature = "serde", serde(rename = "min-bundles"))]
    pub min_bundles: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            capacity: core::num::NonZero::new(256 * 1_048_576).unwrap(),
            min_bundles: 32,
        }
    }
}

struct Inner {
    cache: lru::LruCache<String, (time::OffsetDateTime, Bytes)>,
    capacity: usize,
}

struct Storage {
    inner: Mutex<Inner>,
    max_capacity: core::num::NonZeroUsize,
    min_bundles: usize,
}

#[async_trait]
impl storage::BundleStorage for Storage {
    async fn recover(&self, tx: storage::Sender<storage::RecoveryResponse>) -> storage::Result<()> {
        let snapshot = self
            .inner
            .lock()
            .trace_expect("Failed to lock mutex")
            .cache
            .iter()
            .map(|(n, (t, _))| (n.clone().into(), *t))
            .collect::<Vec<_>>();

        for (name, t) in snapshot {
            tx.send_async((name, t)).await?;
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
            .map(|(_, b)| b.clone()))
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
            .put(
                storage_name.clone(),
                (time::OffsetDateTime::now_utc(), data),
            )
            .map(|(_, d)| d.len())
            .unwrap_or(0);

        // Ensure we cap the total stored, but keep 32 bundles
        inner.capacity = inner
            .capacity
            .saturating_sub(old_len)
            .saturating_add(new_len);
        while inner.cache.len() > self.min_bundles && inner.capacity > self.max_capacity.into() {
            let Some((_, (_, d))) = inner.cache.pop_lru() else {
                break;
            };
            inner.capacity = inner.capacity.saturating_sub(d.len());
        }

        Ok(storage_name.into())
    }

    async fn delete(&self, storage_name: &str) -> storage::Result<()> {
        let mut inner = self.inner.lock().trace_expect("Failed to lock mutex");
        if let Some((_, d)) = inner.cache.pop(storage_name) {
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

#[cfg(test)]
mod tests {
    // use super::*;

    // // TODO: Implement test for 'Eviction Policy (FIFO)' (Verify oldest bundle is dropped on full)
    // #[test]
    // fn test_eviction_policy_fifo() {
    //     todo!("Verify oldest bundle is dropped on full");
    // }

    // // TODO: Implement test for 'Eviction Policy (Priority)' (Verify low priority is dropped for high priority)
    // #[test]
    // fn test_eviction_policy_priority() {
    //     todo!("Verify low priority is dropped for high priority");
    // }

    // // TODO: Implement test for 'Min Bundles Protection' (Verify min_bundles overrides byte quota)
    // #[test]
    // fn test_min_bundles_protection() {
    //     todo!("Verify min_bundles overrides byte quota");
    // }
}
