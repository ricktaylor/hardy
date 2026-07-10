use core::num::{NonZero, NonZeroUsize};

use hardy_async::{async_trait, sync::Mutex};
use hardy_bpv7::{bundle::Id, eid::Eid};
use lru::LruCache;
use tracing::info;

use super::{MetadataStorage, Result};
use crate::{
    bundle::{Bundle, BundleMetadata, BundleStatus},
    stream::Sender,
};

/// Configuration for [`MetadataMemStorage`].
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default, rename_all = "kebab-case"))]
pub struct Config {
    /// Maximum number of entries (live bundles plus tombstones) held before
    /// the store evicts old entries to make room. Default: `1_048_576`.
    pub max_bundles: NonZeroUsize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_bundles: NonZero::new(1_048_576).unwrap(),
        }
    }
}

// A watermark transition detected under the lock, logged after release.
enum Edge {
    Enter { live: usize },
    Exit { live: usize, evicted_live: u64 },
}

struct Inner {
    // Some = live bundle, None = a tombstone remembering a deletion so
    // duplicates of the bundle are not re-accepted.
    entries: LruCache<Id, Option<Bundle>>,
    live: usize,
    tombstones: usize,
    near_capacity: bool,
    evicted_live: u64,
}

impl Inner {
    // Insert or replace `value` under `key`, maintaining the live/tombstone
    // counts and eviction metrics. A capacity eviction takes the LRU entry,
    // which is always the oldest tombstone while any tombstone exists,
    // because tombstoned keys are demoted to the LRU tail and never promoted
    // afterwards. Returns the previous value stored under `key`, if any.
    fn upsert(&mut self, key: Id, value: Option<Bundle>) -> Option<Option<Bundle>> {
        let is_tombstone = value.is_none();
        match &value {
            Some(_) => self.live += 1,
            None => self.tombstones += 1,
        }

        let prev = match self.entries.push(key.clone(), value) {
            Some((k, prev)) if k == key => {
                match &prev {
                    Some(_) => self.live -= 1,
                    None => self.tombstones -= 1,
                }
                Some(prev)
            }
            Some((_, evicted)) => {
                match &evicted {
                    // With no background sweep, expired bundles linger until
                    // evicted or reaped; discarding one is housekeeping, not
                    // data loss, so it stays out of the episode accounting.
                    Some(bundle) if bundle.has_expired() => {
                        self.live -= 1;
                        metrics::counter!("bpa.mem_metadata.evictions", "kind" => "expired")
                            .increment(1);
                    }
                    Some(_) => {
                        self.live -= 1;
                        self.evicted_live += 1;
                        metrics::counter!("bpa.mem_metadata.evictions", "kind" => "live")
                            .increment(1);
                    }
                    None => {
                        self.tombstones -= 1;
                        metrics::counter!("bpa.mem_metadata.evictions", "kind" => "tombstone")
                            .increment(1);
                    }
                }
                None
            }
            None => None,
        };

        if is_tombstone {
            // Tombstones are expendable: sink them to the LRU tail so
            // capacity evictions consume them before any live bundle.
            self.entries.demote(&key);
        }

        metrics::gauge!("bpa.mem_metadata.entries").set(self.live as f64);
        metrics::gauge!("bpa.mem_metadata.tombstones").set(self.tombstones as f64);

        prev
    }

    // Edge-triggered watermark detection with hysteresis: fires once when
    // the live count crosses `high`, and once when it falls back below
    // `low`, however many mutations happen in between.
    fn check_watermark(&mut self, high: usize, low: usize) -> Option<Edge> {
        if !self.near_capacity && self.live >= high {
            self.near_capacity = true;
            Some(Edge::Enter { live: self.live })
        } else if self.near_capacity && self.live < low {
            self.near_capacity = false;
            Some(Edge::Exit {
                live: self.live,
                evicted_live: core::mem::take(&mut self.evicted_live),
            })
        } else {
            None
        }
    }
}

/// An in-memory [`MetadataStorage`] implementation backed by a bounded LRU cache.
///
/// Contents are not persisted: all metadata is lost on restart. When the
/// cache is full, tombstones are evicted in preference to live bundles, and
/// a live bundle is evicted only once no tombstones remain. A single `info!`
/// line is emitted when the live bundle count crosses 95% of capacity, and
/// another when it falls back below 90%, so sustained pressure does not
/// flood the log.
pub struct MetadataMemStorage {
    inner: Mutex<Inner>,
    max_bundles: NonZeroUsize,
    high_watermark: usize,
    low_watermark: usize,
}

impl MetadataMemStorage {
    /// Creates a store holding at most [`Config::max_bundles`] entries.
    pub fn new(config: &Config) -> Self {
        info!(
            "Using in-memory metadata storage (max {} bundles, non-persistent)",
            config.max_bundles
        );

        let max = config.max_bundles.get();

        Self {
            inner: Mutex::new(Inner {
                entries: LruCache::new(config.max_bundles),
                live: 0,
                tombstones: 0,
                near_capacity: false,
                evicted_live: 0,
            }),
            max_bundles: config.max_bundles,
            // 95% and 90%, computed subtractively so the arithmetic cannot
            // overflow usize on 32-bit targets.
            high_watermark: max - max / 20,
            low_watermark: max - max / 10,
        }
    }

    // Apply a mutation, then emit any watermark transition once the lock has
    // been released.
    fn apply(&self, key: Id, value: Option<Bundle>) -> Option<Option<Bundle>> {
        let (prev, edge) = {
            let mut inner = self.inner.lock();
            let prev = inner.upsert(key, value);
            (
                prev,
                inner.check_watermark(self.high_watermark, self.low_watermark),
            )
        };
        self.log_edge(edge);
        prev
    }

    fn log_edge(&self, edge: Option<Edge>) {
        match edge {
            Some(Edge::Enter { live }) => info!(
                "In-memory metadata storage is nearly full: {live} of {} entries are live bundles",
                self.max_bundles
            ),
            Some(Edge::Exit {
                live,
                evicted_live: 0,
            }) => info!(
                "In-memory metadata storage is no longer nearly full: {live} of {} entries are live bundles",
                self.max_bundles
            ),
            Some(Edge::Exit { live, evicted_live }) => info!(
                "In-memory metadata storage is no longer nearly full: {live} of {} entries are live bundles; {evicted_live} live bundles were evicted while nearly full",
                self.max_bundles
            ),
            None => {}
        }
    }

    #[cfg(test)]
    fn near_capacity(&self) -> bool {
        self.inner.lock().near_capacity
    }

    #[cfg(test)]
    fn evicted_live(&self) -> u64 {
        self.inner.lock().evicted_live
    }
}

#[async_trait]
impl MetadataStorage for MetadataMemStorage {
    async fn get(&self, bundle_id: &Id) -> Result<Option<Bundle>> {
        Ok(self.inner.lock().entries.peek(bundle_id).cloned().flatten())
    }

    async fn insert(&self, bundle: &Bundle) -> Result<bool> {
        let key = bundle.bundle.id.clone();
        let edge = {
            let mut inner = self.inner.lock();
            // contains() leaves the LRU order untouched: a duplicate lookup
            // must not promote an existing tombstone off the LRU tail.
            if inner.entries.contains(&key) {
                return Ok(false);
            }
            inner.upsert(key, Some(bundle.clone()));
            inner.check_watermark(self.high_watermark, self.low_watermark)
        };
        self.log_edge(edge);
        Ok(true)
    }

    async fn replace(&self, bundle: &Bundle) -> Result<()> {
        self.apply(bundle.bundle.id.clone(), Some(bundle.clone()));
        Ok(())
    }

    async fn update_status(&self, bundle: &Bundle) -> Result<()> {
        self.replace(bundle).await
    }

    async fn tombstone(&self, bundle_id: &Id) -> Result<()> {
        self.apply(bundle_id.clone(), None);
        Ok(())
    }

    async fn start_recovery(&self) {
        // No-op for in-memory store
    }

    async fn confirm_exists(
        &self,
        _bundle_id: &hardy_bpv7::bundle::Id,
    ) -> Result<Option<BundleMetadata>> {
        Ok(None)
    }

    async fn remove_unconfirmed(&self, _stream: &dyn Sender<Bundle>) -> Result<()> {
        Ok(())
    }

    async fn reset_peer_queue(&self, peer: u32) -> Result<u64> {
        let mut updated = 0;
        for (_, v) in self.inner.lock().entries.iter_mut() {
            if let Some(v) = v
                && let BundleStatus::ForwardPending { peer: p, queue: _ } = v.metadata.status
                && p == peer
            {
                v.metadata.status = BundleStatus::Waiting;
                updated += 1;
            }
        }
        Ok(updated)
    }

    async fn poll_expiry(&self, stream: &dyn Sender<Bundle>, limit: usize) -> Result<()> {
        let mut entries: Vec<Bundle> = self
            .inner
            .lock()
            .entries
            .iter()
            .filter_map(|(_, v)| v.as_ref())
            .filter(|v| v.metadata.status != BundleStatus::New)
            .cloned()
            .collect();

        entries.sort_unstable_by_key(|b| b.expiry());

        for e in entries.into_iter().take(limit) {
            if stream.send(e).await.is_err() {
                break;
            }
        }
        Ok(())
    }

    async fn poll_waiting(&self, stream: &dyn Sender<Bundle>) -> Result<()> {
        let mut entries: Vec<Bundle> = self
            .inner
            .lock()
            .entries
            .iter()
            .filter_map(|(_, v)| v.as_ref())
            .filter(|b| b.metadata.status == BundleStatus::Waiting)
            .cloned()
            .collect();

        entries.sort_unstable_by_key(|b| b.metadata.read_only.received_at);

        for bundle in entries {
            if stream.send(bundle).await.is_err() {
                break;
            }
        }
        Ok(())
    }

    async fn poll_service_waiting(&self, source: Eid, stream: &dyn Sender<Bundle>) -> Result<()> {
        let mut entries: Vec<Bundle> = self
            .inner
            .lock()
            .entries
            .iter()
            .filter_map(|(_, v)| v.as_ref())
            .filter(|b| {
                matches!(&b.metadata.status, BundleStatus::WaitingForService { service } if service == &source)
            })
            .cloned()
            .collect();

        entries.sort_unstable_by_key(|b| b.metadata.read_only.received_at);

        for bundle in entries {
            if stream.send(bundle).await.is_err() {
                break;
            }
        }
        Ok(())
    }

    async fn poll_adu_fragments(
        &self,
        stream: &dyn Sender<Bundle>,
        status: &BundleStatus,
    ) -> Result<()> {
        let mut entries: Vec<(u64, Bundle)> = self
            .inner
            .lock()
            .entries
            .iter()
            .filter_map(|(_, v)| v.as_ref())
            .filter(|v| &v.metadata.status == status)
            .filter_map(|v| {
                v.bundle
                    .id
                    .fragment_info
                    .as_ref()
                    .map(|fi| (fi.offset, v.clone()))
            })
            .collect();

        entries.sort_unstable_by_key(|(offset, _)| *offset);

        for (_, e) in entries {
            if stream.send(e).await.is_err() {
                break;
            }
        }
        Ok(())
    }

    async fn poll_pending(
        &self,
        stream: &dyn Sender<Bundle>,
        state: &BundleStatus,
        limit: usize,
    ) -> Result<()> {
        let mut entries: Vec<Bundle> = self
            .inner
            .lock()
            .entries
            .iter()
            .filter_map(|(_, v)| v.as_ref())
            .filter(|v| &v.metadata.status == state)
            .cloned()
            .collect();

        entries.sort_unstable_by_key(|b| b.metadata.read_only.received_at);

        for e in entries.into_iter().take(limit) {
            if stream.send(e).await.is_err() {
                break;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small_config(max_bundles: usize) -> Config {
        Config {
            max_bundles: NonZeroUsize::new(max_bundles).unwrap(),
        }
    }

    fn make_bundle(n: u32) -> Bundle {
        Bundle {
            bundle: hardy_bpv7::bundle::Bundle {
                id: hardy_bpv7::bundle::Id {
                    source: format!("ipn:0.{n}.1").parse().unwrap(),
                    timestamp: hardy_bpv7::creation_timestamp::CreationTimestamp::now(),
                    fragment_info: None,
                },
                flags: Default::default(),
                crc_type: Default::default(),
                destination: "ipn:0.99.1".parse().unwrap(),
                report_to: Default::default(),
                lifetime: core::time::Duration::from_secs(3600),
                previous_node: None,
                age: None,
                hop_count: None,
                blocks: Default::default(),
            },
            metadata: Default::default(),
        }
    }

    // A full cache must evict a tombstone in preference to a live bundle,
    // even though the tombstone was written more recently.
    #[tokio::test]
    async fn tombstone_evicted_before_live() {
        let storage = MetadataMemStorage::new(&small_config(3));
        let (a, b, c, d) = (
            make_bundle(1),
            make_bundle(2),
            make_bundle(3),
            make_bundle(4),
        );

        assert!(storage.insert(&a).await.unwrap());
        assert!(storage.insert(&b).await.unwrap());
        assert!(storage.insert(&c).await.unwrap());

        storage.tombstone(&a.bundle.id).await.unwrap();

        // The cache is full: inserting d must evict a's tombstone, not the
        // oldest live bundle (b).
        assert!(storage.insert(&d).await.unwrap());
        assert!(storage.get(&b.bundle.id).await.unwrap().is_some());
        assert!(storage.get(&c.bundle.id).await.unwrap().is_some());
        assert!(storage.get(&d.bundle.id).await.unwrap().is_some());
    }

    // With no tombstones to consume, capacity eviction falls back to the
    // least-recently-used live bundle.
    #[tokio::test]
    async fn oldest_live_evicted_when_no_tombstones() {
        let storage = MetadataMemStorage::new(&small_config(2));
        let (a, b, c) = (make_bundle(1), make_bundle(2), make_bundle(3));

        assert!(storage.insert(&a).await.unwrap());
        assert!(storage.insert(&b).await.unwrap());
        assert!(storage.insert(&c).await.unwrap());

        assert!(storage.get(&a.bundle.id).await.unwrap().is_none());
        assert!(storage.get(&b.bundle.id).await.unwrap().is_some());
        assert!(storage.get(&c.bundle.id).await.unwrap().is_some());
    }

    // A duplicate of a tombstoned bundle is refused, and the refusal must
    // not promote the tombstone off the LRU tail.
    #[tokio::test]
    async fn reinsert_of_tombstoned_id_refused_without_promotion() {
        let storage = MetadataMemStorage::new(&small_config(3));
        let (a, b, c, d) = (
            make_bundle(1),
            make_bundle(2),
            make_bundle(3),
            make_bundle(4),
        );

        assert!(storage.insert(&a).await.unwrap());
        assert!(storage.insert(&b).await.unwrap());
        assert!(storage.insert(&c).await.unwrap());

        storage.tombstone(&a.bundle.id).await.unwrap();
        assert!(!storage.insert(&a).await.unwrap());

        // The tombstone must still be the next eviction victim.
        assert!(storage.insert(&d).await.unwrap());
        assert!(storage.get(&b.bundle.id).await.unwrap().is_some());

        // The tombstone is gone with it, so a duplicate of a is accepted again.
        assert!(storage.insert(&a).await.unwrap());
    }

    fn make_expired_bundle(n: u32) -> Bundle {
        let mut b = make_bundle(n);
        b.bundle.lifetime = core::time::Duration::from_secs(0);
        // Set received_at in the past so expiry is already passed
        b.metadata.read_only.received_at =
            time::OffsetDateTime::now_utc() - time::Duration::seconds(10);
        b
    }

    // Evicting a bundle that has already expired is housekeeping, not data
    // loss: it must not count towards the episode's evicted-live tally.
    #[tokio::test]
    async fn expired_eviction_is_not_data_loss() {
        let storage = MetadataMemStorage::new(&small_config(2));
        let (a, b, c, d) = (
            make_expired_bundle(1),
            make_bundle(2),
            make_bundle(3),
            make_bundle(4),
        );

        assert!(storage.insert(&a).await.unwrap());
        assert!(storage.insert(&b).await.unwrap());

        // Evicts a, which is already expired
        assert!(storage.insert(&c).await.unwrap());
        assert_eq!(storage.evicted_live(), 0);

        // Evicts b, which is live and unexpired
        assert!(storage.insert(&d).await.unwrap());
        assert_eq!(storage.evicted_live(), 1);
    }

    // The episode is entered once at the high watermark and left once below
    // the low watermark; the band between the two does not flap.
    #[tokio::test]
    async fn watermark_edges_are_hysteretic() {
        // max 20: high watermark = 19 live, low watermark = 18 live
        let storage = MetadataMemStorage::new(&small_config(20));
        let bundles: Vec<Bundle> = (1..=19).map(make_bundle).collect();

        for b in &bundles[..18] {
            storage.insert(b).await.unwrap();
            assert!(!storage.near_capacity());
        }
        storage.insert(&bundles[18]).await.unwrap();
        assert!(storage.near_capacity(), "19 of 20 live crosses 95%");

        // 18 live == low watermark: still inside the hysteresis band
        storage.tombstone(&bundles[0].bundle.id).await.unwrap();
        assert!(storage.near_capacity());

        // 17 live < 18 exits the episode
        storage.tombstone(&bundles[1].bundle.id).await.unwrap();
        assert!(!storage.near_capacity());
    }
}
