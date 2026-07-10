use core::num::{NonZero, NonZeroUsize};

use hardy_async::{async_trait, sync::Mutex};
use lru::LruCache;
use rand::{
    SeedableRng,
    distr::{Alphanumeric, SampleString},
    rngs::{SmallRng, SysRng},
};
use time::OffsetDateTime;
use tracing::{info, warn};

use super::{BundleStorage, RecoveryResponse, Result};
use crate::{Arc, Bytes, stream::Sender};

/// Configuration for [`BundleMemStorage`].
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default, rename_all = "kebab-case"))]
pub struct Config {
    /// Maximum total bytes of bundle data held before least-recently-used
    /// bundles are evicted. Default: 256 MiB.
    pub capacity: NonZeroUsize,

    /// Minimum number of bundles retained regardless of the byte capacity.
    /// Values below 1 are treated as 1, so a save can never evict the bundle
    /// it has just stored. Default: `32`.
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

// A watermark transition detected under the lock, logged after release.
enum Edge {
    Enter {
        bytes: usize,
    },
    Exit {
        bytes: usize,
        evicted_count: u64,
        evicted_bytes: u64,
    },
}

struct Inner {
    cache: LruCache<String, (OffsetDateTime, Bytes)>,
    capacity: usize,
    rng: SmallRng,
    near_capacity: bool,
    evicted_count: u64,
    evicted_bytes: u64,
}

impl Inner {
    // Evict least-recently-used bundles until usage is back within
    // `max_capacity`, never dropping below `min_bundles` entries. Evictions
    // are accumulated into the episode counters for the watermark exit line.
    fn evict_to_capacity(&mut self, max_capacity: usize, min_bundles: usize) {
        while self.cache.len() > min_bundles && self.capacity > max_capacity {
            let Some((_, (_, d))) = self.cache.pop_lru() else {
                break;
            };
            self.capacity = self.capacity.saturating_sub(d.len());
            self.evicted_count += 1;
            self.evicted_bytes += d.len() as u64;
            metrics::counter!("bpa.mem_store.evictions").increment(1);
        }
    }

    // Edge-triggered watermark detection with hysteresis: fires once when
    // usage crosses `high`, and once when it falls back below `low`,
    // however many mutations happen in between.
    fn check_watermark(&mut self, high: usize, low: usize) -> Option<Edge> {
        if !self.near_capacity && self.capacity >= high {
            self.near_capacity = true;
            Some(Edge::Enter {
                bytes: self.capacity,
            })
        } else if self.near_capacity && self.capacity < low {
            self.near_capacity = false;
            Some(Edge::Exit {
                bytes: self.capacity,
                evicted_count: core::mem::take(&mut self.evicted_count),
                evicted_bytes: core::mem::take(&mut self.evicted_bytes),
            })
        } else {
            None
        }
    }
}

/// An in-memory [`BundleStorage`] implementation bounded by total byte
/// capacity.
///
/// Contents are not persisted: all bundle data is lost on restart. When
/// usage exceeds the configured capacity, least-recently-used bundles are
/// evicted — and since this store holds the only copy, eviction discards
/// the bundle. A single `info!` line is emitted when usage crosses 95% of
/// capacity, and another when it falls back below 90%, so sustained
/// pressure does not flood the log.
pub struct BundleMemStorage {
    inner: Mutex<Inner>,
    max_capacity: NonZeroUsize,
    min_bundles: usize,
    high_watermark: usize,
    low_watermark: usize,
}

impl BundleMemStorage {
    /// Creates a store holding at most [`Config::capacity`] bytes.
    pub fn new(config: &Config) -> Self {
        warn!(
            "Using in-memory bundle storage (capacity {} bytes): stored bundles will NOT survive a restart",
            config.capacity
        );

        let inner = Mutex::new(Inner {
            cache: LruCache::unbounded(),
            capacity: 0,
            rng: SmallRng::try_from_rng(&mut SysRng)
                .expect("OS RNG must be available to seed the storage-name PRNG"),
            near_capacity: false,
            evicted_count: 0,
            evicted_bytes: 0,
        });
        let max_capacity = config.capacity;
        let max = max_capacity.get();

        Self {
            inner,
            max_capacity,
            // A floor of one entry guarantees the eviction loop can never
            // discard the bundle a save has just stored.
            min_bundles: config.min_bundles.max(1),
            // 95% and 90%, computed subtractively so the arithmetic cannot
            // overflow usize on 32-bit targets.
            high_watermark: max - max / 20,
            low_watermark: max - max / 10,
        }
    }

    fn log_edge(&self, edge: Option<Edge>) {
        match edge {
            Some(Edge::Enter { bytes }) => info!(
                "In-memory bundle storage is nearly full: {bytes} of {} bytes used",
                self.max_capacity
            ),
            Some(Edge::Exit {
                bytes,
                evicted_count: 0,
                ..
            }) => info!(
                "In-memory bundle storage is no longer nearly full: {bytes} of {} bytes used",
                self.max_capacity
            ),
            Some(Edge::Exit {
                bytes,
                evicted_count,
                evicted_bytes,
            }) => info!(
                "In-memory bundle storage is no longer nearly full: {bytes} of {} bytes used; {evicted_count} bundles ({evicted_bytes} bytes) were evicted while nearly full",
                self.max_capacity
            ),
            None => {}
        }
    }

    #[cfg(test)]
    fn near_capacity(&self) -> bool {
        self.inner.lock().near_capacity
    }

    #[cfg(test)]
    fn evicted_count(&self) -> u64 {
        self.inner.lock().evicted_count
    }
}

#[async_trait]
impl BundleStorage for BundleMemStorage {
    async fn recover(&self, stream: &dyn Sender<RecoveryResponse>) -> Result<()> {
        let snapshot = self
            .inner
            .lock()
            .cache
            .iter()
            .map(|(n, (t, _))| (n.clone().into(), *t))
            .collect::<Vec<_>>();

        for (name, t) in snapshot {
            if stream.send((name, t)).await.is_err() {
                break;
            }
        }
        Ok(())
    }

    // Bytes::clone is a refcount bump, not a data copy. The entry stays in
    // the cache until delete() so the forwarding retry paths can load the
    // data again after a failed CLA send. Because the cache retains a
    // reference, editors rewriting loaded data take the copying
    // Chunk::flatten path rather than mutating the buffer in place via
    // try_into_mut() — the retained copy must survive the rewrite.
    async fn load(&self, storage_name: &str) -> Result<Option<Bytes>> {
        Ok(self
            .inner
            .lock()
            .cache
            .get(storage_name)
            .map(|(_, data)| data.clone()))
    }

    async fn save(&self, data: Bytes) -> Result<Arc<str>> {
        let new_len = data.len();

        let (storage_name, e1, e2) = loop {
            let mut inner = self.inner.lock();
            // Storage names only need to be unique, not unpredictable.
            let storage_name = Alphanumeric.sample_string(&mut inner.rng, 64);
            if inner.cache.contains(&storage_name) {
                continue;
            }

            let old_len = inner
                .cache
                .put(storage_name.clone(), (OffsetDateTime::now_utc(), data))
                .map(|(_, d)| d.len())
                .unwrap_or(0);

            inner.capacity = inner
                .capacity
                .saturating_sub(old_len)
                .saturating_add(new_len);

            // Check the enter edge at peak usage, before eviction pulls it
            // back down: a large overshoot can drop straight through the
            // hysteresis band, which the second check reports as an exit.
            let e1 = inner.check_watermark(self.high_watermark, self.low_watermark);
            inner.evict_to_capacity(self.max_capacity.into(), self.min_bundles);
            let e2 = inner.check_watermark(self.high_watermark, self.low_watermark);

            metrics::gauge!("bpa.mem_store.bundles").set(inner.cache.len() as f64);
            metrics::gauge!("bpa.mem_store.bytes").set(inner.capacity as f64);

            break (storage_name, e1, e2);
        };

        self.log_edge(e1);
        self.log_edge(e2);

        Ok(storage_name.into())
    }

    async fn replace(&self, storage_name: &str, data: Bytes) -> Result<()> {
        let new_len = data.len();
        let (e1, e2) = {
            let mut inner = self.inner.lock();
            let old_len = inner
                .cache
                .put(storage_name.to_string(), (OffsetDateTime::now_utc(), data))
                .map(|(_, d)| d.len())
                .unwrap_or(0);
            inner.capacity = inner
                .capacity
                .saturating_sub(old_len)
                .saturating_add(new_len);

            let e1 = inner.check_watermark(self.high_watermark, self.low_watermark);
            inner.evict_to_capacity(self.max_capacity.into(), self.min_bundles);
            let e2 = inner.check_watermark(self.high_watermark, self.low_watermark);

            metrics::gauge!("bpa.mem_store.bundles").set(inner.cache.len() as f64);
            metrics::gauge!("bpa.mem_store.bytes").set(inner.capacity as f64);
            (e1, e2)
        };
        self.log_edge(e1);
        self.log_edge(e2);
        Ok(())
    }

    async fn delete(&self, storage_name: &str) -> Result<()> {
        let edge = {
            let mut inner = self.inner.lock();
            let Some((_, d)) = inner.cache.pop(storage_name) else {
                return Ok(());
            };
            inner.capacity = inner.capacity.saturating_sub(d.len());
            metrics::gauge!("bpa.mem_store.bundles").set(inner.cache.len() as f64);
            metrics::gauge!("bpa.mem_store.bytes").set(inner.capacity as f64);
            inner.check_watermark(self.high_watermark, self.low_watermark)
        };
        self.log_edge(edge);
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

    // When capacity is exceeded, the oldest-inserted bundle is evicted.
    #[tokio::test]
    async fn test_eviction_policy_fifo() {
        // 100 bytes capacity, min 0 bundles (so eviction is purely capacity-driven)
        let storage = BundleMemStorage::new(&small_config(100, 0));

        // Insert 3 bundles of 50 bytes each — total 150 > 100, so eviction should occur
        let name1 = storage.save(Bytes::from(vec![1u8; 50])).await.unwrap();
        let name2 = storage.save(Bytes::from(vec![2u8; 50])).await.unwrap();

        // At this point, capacity = 100, exactly at limit
        let name3 = storage.save(Bytes::from(vec![3u8; 50])).await.unwrap();

        // name3 pushed capacity to 150 > 100, so oldest (name1) should be evicted
        assert!(
            storage.load(&name1).await.unwrap().is_none(),
            "Oldest bundle should be evicted"
        );
        assert!(storage.load(&name2).await.unwrap().is_some());
        assert!(storage.load(&name3).await.unwrap().is_some());
    }

    // Eviction is strictly FIFO by insertion order.
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

    // A save must never evict the bundle it has just stored, even when that
    // bundle alone exceeds the whole byte capacity (min_bundles clamps to 1).
    #[tokio::test]
    async fn save_survives_its_own_eviction_pass() {
        let storage = BundleMemStorage::new(&small_config(100, 0));

        let name1 = storage.save(Bytes::from(vec![1u8; 50])).await.unwrap();
        let name2 = storage.save(Bytes::from(vec![2u8; 150])).await.unwrap();

        assert!(storage.load(&name1).await.unwrap().is_none());
        assert!(
            storage.load(&name2).await.unwrap().is_some(),
            "The just-saved bundle must survive its own eviction pass"
        );
    }

    // replace() must enforce the byte capacity, not just account for it.
    #[tokio::test]
    async fn replace_evicts_over_capacity() {
        let storage = BundleMemStorage::new(&small_config(100, 0));

        let name1 = storage.save(Bytes::from(vec![1u8; 50])).await.unwrap();
        let name2 = storage.save(Bytes::from(vec![2u8; 50])).await.unwrap();

        // Growing name2 to 90 bytes pushes usage to 140: name1 (LRU) must go
        storage
            .replace(&name2, Bytes::from(vec![3u8; 90]))
            .await
            .unwrap();

        assert!(storage.load(&name1).await.unwrap().is_none());
        assert_eq!(storage.load(&name2).await.unwrap().unwrap().len(), 90);
    }

    // The episode is entered once at the high watermark and left once below
    // the low watermark; the band between the two does not flap.
    #[tokio::test]
    async fn watermark_edges_are_hysteretic() {
        // capacity 1000: high watermark = 950 bytes, low watermark = 900
        let storage = BundleMemStorage::new(&small_config(1000, 1));

        let _name1 = storage.save(Bytes::from(vec![1u8; 500])).await.unwrap();
        let name2 = storage.save(Bytes::from(vec![2u8; 440])).await.unwrap();
        assert!(!storage.near_capacity(), "940 of 1000 is below 95%");

        let name3 = storage.save(Bytes::from(vec![3u8; 50])).await.unwrap();
        assert!(storage.near_capacity(), "990 of 1000 crosses 95%");

        // 940 bytes == inside the hysteresis band: still near capacity
        storage.delete(&name3).await.unwrap();
        assert!(storage.near_capacity());

        // 500 bytes < 900 exits the episode; nothing was ever evicted
        storage.delete(&name2).await.unwrap();
        assert!(!storage.near_capacity());
        assert_eq!(storage.evicted_count(), 0);
    }

    // Evictions during an episode are tallied and reset when it ends.
    #[tokio::test]
    async fn exit_resets_episode_eviction_tally() {
        let storage = BundleMemStorage::new(&small_config(1000, 1));

        let _name1 = storage.save(Bytes::from(vec![1u8; 320])).await.unwrap();
        let _name2 = storage.save(Bytes::from(vec![2u8; 320])).await.unwrap();
        let _name3 = storage.save(Bytes::from(vec![3u8; 320])).await.unwrap();
        assert!(storage.near_capacity(), "960 of 1000 crosses 95%");

        // 1280 bytes forces name1 out; 960 remains, inside the band
        let name4 = storage.save(Bytes::from(vec![4u8; 320])).await.unwrap();
        assert!(storage.near_capacity());
        assert_eq!(storage.evicted_count(), 1);

        // 640 bytes < 900 exits the episode and resets the tally
        storage.delete(&name4).await.unwrap();
        assert!(!storage.near_capacity());
        assert_eq!(storage.evicted_count(), 0);
    }

    // A save that overshoots capacity can evict so much that usage falls
    // straight through the hysteresis band: the episode opens and closes
    // within the one call, and the tally is reported and reset by the exit.
    #[tokio::test]
    async fn overshoot_enters_and_exits_in_one_save() {
        let storage = BundleMemStorage::new(&small_config(1000, 1));

        let _name1 = storage.save(Bytes::from(vec![1u8; 600])).await.unwrap();
        // 1200 bytes crosses 95%, then evicting name1 drops usage to 600,
        // below the 90% low watermark
        let _name2 = storage.save(Bytes::from(vec![2u8; 600])).await.unwrap();

        assert!(!storage.near_capacity());
        assert_eq!(storage.evicted_count(), 0, "tally consumed by the exit");
    }
}
