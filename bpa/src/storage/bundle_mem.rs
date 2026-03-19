use core::num::{NonZero, NonZeroUsize};

use bytes::Bytes;
use hardy_async::async_trait;
use hardy_async::sync::Mutex;
use rand::distr::{Alphanumeric, SampleString};
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use tracing::info;

use super::{BundleStorage, RecoveryResponse, Result, Sender};
use crate::Arc;

#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(default, rename_all = "kebab-case"))]
pub struct Config {
    pub capacity: NonZeroUsize,
    pub min_bundles: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            capacity: NonZero::new(256 * 1_048_576).unwrap(),
            min_bundles: 32,
        }
    }
}

struct Inner {
    cache: lru::LruCache<String, (time::OffsetDateTime, Bytes)>,
    capacity: usize,
}

pub struct BundleMemStorage {
    inner: Mutex<Inner>,
    max_capacity: NonZeroUsize,
    min_bundles: usize,
}

impl BundleMemStorage {
    pub fn new(config: &Config) -> Self {
        info!(
            "Using in-memory bundle storage (capacity {} bytes, non-persistent)",
            config.capacity
        );

        let inner = Mutex::new(Inner {
            cache: lru::LruCache::unbounded(),
            capacity: 0,
        });
        let max_capacity = config.capacity;
        let min_bundles = config.min_bundles;

        Self {
            inner,
            max_capacity,
            min_bundles,
        }
    }
}

#[async_trait]
impl BundleStorage for BundleMemStorage {
    async fn recover(&self, tx: Sender<RecoveryResponse>) -> Result<()> {
        let snapshot = self
            .inner
            .lock()
            .cache
            .iter()
            .map(|(n, (t, _))| (n.clone().into(), *t))
            .collect::<Vec<_>>();

        for (name, t) in snapshot {
            tx.send_async((name, t)).await?;
        }
        Ok(())
    }

    async fn load(&self, storage_name: &str) -> Result<Option<Bytes>> {
        Ok(self
            .inner
            .lock()
            .cache
            .peek(storage_name)
            .map(|(_, b)| b.clone()))
    }

    async fn save(&self, data: Bytes) -> Result<Arc<str>> {
        let mut rng = rand::rng();
        let mut inner = self.inner.lock();
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

    async fn delete(&self, storage_name: &str) -> Result<()> {
        let mut inner = self.inner.lock();
        if let Some((_, d)) = inner.cache.pop(storage_name) {
            inner.capacity = inner.capacity.saturating_sub(d.len());
        }
        Ok(())
    }
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
