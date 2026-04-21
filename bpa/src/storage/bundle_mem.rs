use core::num::{NonZero, NonZeroUsize};

use flume::Sender;
use hardy_async::async_trait;
use hardy_async::sync::Mutex;
use rand::distr::{Alphanumeric, SampleString};
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use tracing::info;

use super::{BundleStorage, RecoveryResponse, Result};
use crate::{Arc, Bytes};

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
        let new_len = data.len();

        loop {
            let storage_name = Alphanumeric.sample_string(&mut rng, 64);

            let mut inner = self.inner.lock();
            if inner.cache.contains(&storage_name) {
                continue;
            }

            let old_len = inner
                .cache
                .put(
                    storage_name.clone(),
                    (time::OffsetDateTime::now_utc(), data),
                )
                .map(|(_, d)| d.len())
                .unwrap_or(0);

            inner.capacity = inner
                .capacity
                .saturating_sub(old_len)
                .saturating_add(new_len);
            while inner.cache.len() > self.min_bundles && inner.capacity > self.max_capacity.into()
            {
                let Some((_, (_, d))) = inner.cache.pop_lru() else {
                    break;
                };
                inner.capacity = inner.capacity.saturating_sub(d.len());
                metrics::counter!("bpa.mem_store.evictions").increment(1);
            }

            metrics::gauge!("bpa.mem_store.bundles").set(inner.cache.len() as f64);
            metrics::gauge!("bpa.mem_store.bytes").set(inner.capacity as f64);

            return Ok(storage_name.into());
        }
    }

    async fn overwrite(&self, storage_name: &str, data: Bytes) -> Result<()> {
        let mut inner = self.inner.lock();
        let new_len = data.len();
        let old_len = inner
            .cache
            .put(
                storage_name.to_string(),
                (time::OffsetDateTime::now_utc(), data),
            )
            .map(|(_, d)| d.len())
            .unwrap_or(0);
        inner.capacity = inner
            .capacity
            .saturating_sub(old_len)
            .saturating_add(new_len);
        metrics::gauge!("bpa.mem_store.bytes").set(inner.capacity as f64);
        Ok(())
    }

    async fn delete(&self, storage_name: &str) -> Result<()> {
        let mut inner = self.inner.lock();
        if let Some((_, d)) = inner.cache.pop(storage_name) {
            inner.capacity = inner.capacity.saturating_sub(d.len());
            metrics::gauge!("bpa.mem_store.bundles").set(inner.cache.len() as f64);
            metrics::gauge!("bpa.mem_store.bytes").set(inner.capacity as f64);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small_config(capacity: usize, min_bundles: usize) -> Config {
        Config {
            capacity: NonZeroUsize::new(capacity).unwrap(),
            min_bundles,
        }
    }

    // When capacity is exceeded, the LRU (oldest-accessed) bundle is evicted.
    #[tokio::test]
    async fn test_eviction_policy_fifo() {
        // 100 bytes capacity, min 0 bundles (so eviction is purely capacity-driven)
        let storage = BundleMemStorage::new(&small_config(100, 0));

        // Insert 3 bundles of 50 bytes each — total 150 > 100, so eviction should occur
        let name1 = storage.save(Bytes::from(vec![1u8; 50])).await.unwrap();
        let name2 = storage.save(Bytes::from(vec![2u8; 50])).await.unwrap();

        // At this point, capacity = 100, exactly at limit
        let name3 = storage.save(Bytes::from(vec![3u8; 50])).await.unwrap();

        // name3 pushed capacity to 150 > 100, so LRU (name1) should be evicted
        assert!(
            storage.load(&name1).await.unwrap().is_none(),
            "Oldest bundle should be evicted"
        );
        assert!(storage.load(&name2).await.unwrap().is_some());
        assert!(storage.load(&name3).await.unwrap().is_some());
    }

    // BundleMemStorage uses insertion-order LRU. load() uses peek() so does NOT
    // promote entries. Eviction is strictly FIFO by insertion order.
    #[tokio::test]
    async fn test_eviction_policy_priority() {
        let storage = BundleMemStorage::new(&small_config(100, 0));

        // Insert two bundles (50 bytes each, total 100 = at capacity)
        let name1 = storage.save(Bytes::from(vec![0xFFu8; 50])).await.unwrap();
        let name2 = storage.save(Bytes::from(vec![0x00u8; 50])).await.unwrap();

        // Insert a third — pushes over capacity, evicts oldest (name1)
        let name3 = storage.save(Bytes::from(vec![0xABu8; 50])).await.unwrap();

        // name1 was inserted first (oldest), so it gets evicted
        assert!(
            storage.load(&name1).await.unwrap().is_none(),
            "Oldest insertion should be evicted"
        );
        assert!(
            storage.load(&name2).await.unwrap().is_some(),
            "Second insertion should survive"
        );
        assert!(storage.load(&name3).await.unwrap().is_some());
    }

    // When min_bundles is set, eviction should not reduce count below that threshold
    // even if byte capacity is exceeded.
    #[tokio::test]
    async fn test_min_bundles_protection() {
        // 100 bytes capacity, but min 3 bundles — count protection overrides byte quota
        let storage = BundleMemStorage::new(&small_config(100, 3));

        let name1 = storage.save(Bytes::from(vec![1u8; 50])).await.unwrap();
        let name2 = storage.save(Bytes::from(vec![2u8; 50])).await.unwrap();
        let name3 = storage.save(Bytes::from(vec![3u8; 50])).await.unwrap();

        // Total capacity = 150 > 100, but we have exactly min_bundles (3) entries
        // So no eviction should occur despite exceeding byte capacity
        assert!(
            storage.load(&name1).await.unwrap().is_some(),
            "min_bundles should protect from eviction"
        );
        assert!(storage.load(&name2).await.unwrap().is_some());
        assert!(storage.load(&name3).await.unwrap().is_some());
    }

    // Verify NonZeroUsize handles >1TB capacity values without overflow.
    #[test]
    fn test_large_quota_config() {
        let two_tb: usize = 2_000_000_000_000;
        let config = Config {
            capacity: NonZeroUsize::new(two_tb).unwrap(),
            min_bundles: 0,
        };
        let storage = BundleMemStorage::new(&config);
        assert_eq!(storage.max_capacity.get(), two_tb);
    }
}
