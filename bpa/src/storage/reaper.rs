//! Bundle lifetime expiration monitoring (Reaper).
//!
//! The reaper monitors bundle lifetimes and triggers deletion when bundles
//! expire. It maintains a bounded in-memory cache of the bundles with the
//! soonest expiry times, refilling from storage when depleted.
//!
//! # Two-Level Architecture
//!
//! - **In-memory cache**: BTreeSet of `CacheEntry` ordered by expiry time
//! - **Persistent storage**: MetadataStorage.poll_expiry() for refill
//!
//! The cache keeps bundles with the soonest expiry. When full, entries with
//! later expiry times are evicted to make room for sooner ones.
//!
//! See [Storage Subsystem Design](../../docs/storage_subsystem_design.md)
//! for architectural context.

use core::cmp::Ordering;

use futures::{FutureExt, join, select_biased};
use hardy_async::{Notify, TaskPool, sync::Mutex};
use hardy_bpv7::{bundle::Id, eid::Eid};
use time::OffsetDateTime;
use tracing::{debug, error};

use crate::{
    Arc, BTreeSet,
    bundle::{Bundle, BundleStatus},
    dispatcher::Dispatcher,
};

/// Cache entry for the reaper's expiry monitoring.
///
/// Ordered by: expiry time -> destination -> bundle ID.
#[derive(Clone, Eq, PartialEq)]
pub(super) struct CacheEntry {
    expiry: OffsetDateTime,
    id: Id,
    destination: Eid,
}

impl PartialOrd for CacheEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for CacheEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.expiry
            .cmp(&other.expiry)
            .then_with(|| self.destination.cmp(&other.destination))
            .then_with(|| self.id.cmp(&other.id))
    }
}

/// Monitors bundle lifetimes and triggers deletion when bundles expire.
///
/// Maintains a bounded in-memory cache of bundles with the soonest
/// expiry times, refilling from storage when depleted.
pub(super) struct Reaper {
    tasks: TaskPool,
    metadata_storage: Arc<dyn super::MetadataStorage>,
    cache: Mutex<BTreeSet<CacheEntry>>,
    wakeup: Notify,
    cache_size: usize,
}

impl Reaper {
    pub fn new(
        tasks: TaskPool,
        metadata_storage: Arc<dyn super::MetadataStorage>,
        cache_size: usize,
    ) -> Self {
        Self {
            tasks,
            metadata_storage,
            cache: Mutex::new(BTreeSet::new()),
            wakeup: Notify::new(),
            cache_size,
        }
    }

    /// Add a bundle to the reaper's cache.
    pub fn watch(&self, bundle: &Bundle, cap: bool) {
        let new_entry = CacheEntry {
            expiry: bundle.expiry(),
            id: bundle.bundle.id.clone(),
            destination: bundle.bundle.destination.clone(),
        };

        let new_expiry = new_entry.expiry;
        let old_expiry = {
            let mut cache = self.cache.lock();
            let old_expiry = cache.first().map(|e| e.expiry);

            if !cap || cache.len() < self.cache_size {
                if !cache.insert(new_entry) {
                    return;
                }
            } else {
                let last_expiry = cache.last().map(|e| e.expiry).unwrap();
                if new_expiry < last_expiry {
                    cache.pop_last();
                    if !cache.insert(new_entry) {
                        return;
                    }
                } else {
                    return;
                }
            }
            old_expiry
        };

        if match old_expiry {
            None => true,
            Some(old) => new_expiry < old,
        } {
            self.wakeup.notify_one();
        }
    }

    /// Background task for bundle lifetime monitoring.
    ///
    /// # Behavior
    ///
    /// 1. Sleep until the next bundle expiry (or indefinitely if cache empty)
    /// 2. Wake on: shutdown signal, new bundle notification, or expiry timeout
    /// 3. Expire all bundles past their lifetime via `drop_bundle()`
    /// 4. Spawn `refill_cache()` if cache is depleted
    ///
    /// Uses `select_biased!` to prioritize shutdown handling.
    pub async fn run(self: &Arc<Self>, dispatcher: Arc<Dispatcher>) {
        let mut repopulation_task: Option<hardy_async::JoinHandle<()>> = None;

        loop {
            let sleep_duration = self
                .cache
                .lock()
                .first()
                .map(|entry| entry.expiry - OffsetDateTime::now_utc())
                .unwrap_or(time::Duration::MAX);

            select_biased! {
                _ = self.tasks.cancel_token().cancelled().fuse() => {
                    debug!("Reaper task complete");
                    break;
                }
                _ = self.wakeup.notified().fuse() => {},
                _ = hardy_async::time::sleep(sleep_duration).fuse() => {},
            }

            let mut dead_bundle_ids = Vec::new();
            let check_store = {
                let mut cache = self.cache.lock();
                let now = OffsetDateTime::now_utc();
                while let Some(entry) = cache.first() {
                    if entry.expiry >= now {
                        break;
                    }
                    dead_bundle_ids.push(cache.pop_first().unwrap().id);
                }
                cache.is_empty()
            };

            for id in dead_bundle_ids {
                if let Ok(Some(bundle)) = self
                    .metadata_storage
                    .get(&id)
                    .await
                    .inspect_err(|e| error!("Failed to get metadata from store: {e}"))
                {
                    dispatcher
                        .drop_bundle(
                            bundle,
                            hardy_bpv7::status_report::ReasonCode::LifetimeExpired,
                        )
                        .await;
                }
            }

            if check_store {
                if let Some(handle) = &repopulation_task
                    && !handle.is_finished()
                {
                    continue;
                }

                let reaper = self.clone();
                repopulation_task = Some(hardy_async::spawn!(
                    self.tasks,
                    "refill_cache_task",
                    async move { reaper.refill_cache().await }
                ));
            }
        }
    }

    /// Expose a snapshot of the cache, in expiry order, for test assertions.
    #[cfg(test)]
    fn cache_snapshot(&self) -> Vec<CacheEntry> {
        self.cache.lock().iter().cloned().collect()
    }

    async fn refill_cache(&self) {
        let cancel_token = self.tasks.cancel_token().clone();
        let (stream, rx) = hardy_async::channel::bounded::<Bundle>(self.cache_size);

        join!(
            async {
                // Race against cancel so the producer can't block on a full
                // channel after the consumer breaks (join! keeps rx alive).
                select_biased! {
                    r = self.metadata_storage.poll_expiry(&stream, self.cache_size).fuse() => {
                        let _ = r.inspect_err(|e| error!("Failed to poll store for expiry bundles: {e}"));
                    }
                    _ = cancel_token.cancelled().fuse() => {}
                }
                drop(stream);
            },
            async {
                loop {
                    select_biased! {
                        bundle = rx.recv().fuse() => {
                            let Ok(bundle) = bundle else {
                                break;
                            };
                            if bundle.metadata.status != BundleStatus::New {
                                self.watch(&bundle, false);
                            }
                        },
                        _ = cancel_token.cancelled().fuse() => {
                            break;
                        }
                    }
                }
            }
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MetadataMemStorage;

    fn make_entry(secs_from_now: i64, node: u32) -> CacheEntry {
        CacheEntry {
            expiry: OffsetDateTime::now_utc() + time::Duration::seconds(secs_from_now),
            id: Id {
                source: format!("ipn:0.{node}.1").parse().unwrap(),
                timestamp: hardy_bpv7::creation_timestamp::CreationTimestamp::now(),
                fragment_info: None,
            },
            destination: format!("ipn:0.{node}.99").parse().unwrap(),
        }
    }

    // A bundle expiring `lifetime_secs` from now, with a per-node unique id.
    fn make_bundle(lifetime_secs: u64, node: u32) -> Bundle {
        Bundle {
            bundle: hardy_bpv7::bundle::Bundle {
                id: Id {
                    source: format!("ipn:0.{node}.1").parse().unwrap(),
                    timestamp: hardy_bpv7::creation_timestamp::CreationTimestamp::now(),
                    fragment_info: None,
                },
                flags: Default::default(),
                crc_type: Default::default(),
                destination: format!("ipn:0.{node}.99").parse().unwrap(),
                report_to: Default::default(),
                lifetime: core::time::Duration::from_secs(lifetime_secs),
                previous_node: None,
                age: None,
                hop_count: None,
                blocks: Default::default(),
            },
            metadata: Default::default(),
        }
    }

    fn make_reaper(cache_size: usize) -> Reaper {
        Reaper::new(
            TaskPool::new(),
            Arc::new(MetadataMemStorage::new(None)),
            cache_size,
        )
    }

    #[test]
    fn test_cache_ordering() {
        let mut set = BTreeSet::new();
        let later = make_entry(300, 1);
        let sooner = make_entry(60, 2);
        let middle = make_entry(180, 3);

        set.insert(later.clone());
        set.insert(sooner.clone());
        set.insert(middle.clone());

        let entries: Vec<_> = set.into_iter().collect();
        assert_eq!(entries[0].expiry, sooner.expiry);
        assert_eq!(entries[1].expiry, middle.expiry);
        assert_eq!(entries[2].expiry, later.expiry);
    }

    // When the capped cache is full, watch() must keep the soonest-expiring
    // bundles: a sooner bundle evicts the latest entry.
    #[test]
    fn test_cache_saturation() {
        let reaper = make_reaper(3);

        let b100 = make_bundle(100, 1);
        let b200 = make_bundle(200, 2);
        let b300 = make_bundle(300, 3);
        reaper.watch(&b100, true);
        reaper.watch(&b200, true);
        reaper.watch(&b300, true);
        assert_eq!(reaper.cache_snapshot().len(), 3);

        let b50 = make_bundle(50, 4);
        reaper.watch(&b50, true);

        let entries = reaper.cache_snapshot();
        assert_eq!(entries.len(), 3, "Cache must stay at its cap");
        assert_eq!(
            entries[0].id, b50.bundle.id,
            "Sooner bundle must become the cache head"
        );
        assert!(
            entries.iter().all(|e| e.id != b300.bundle.id),
            "Latest-expiring entry must be evicted"
        );
    }

    // When the capped cache is full, watch() must reject a bundle that
    // expires later than every cached entry; without the cap the same
    // bundle is accepted.
    #[test]
    fn test_cache_rejection() {
        let reaper = make_reaper(3);

        reaper.watch(&make_bundle(100, 1), true);
        reaper.watch(&make_bundle(200, 2), true);
        reaper.watch(&make_bundle(300, 3), true);

        let b400 = make_bundle(400, 4);
        reaper.watch(&b400, true);

        let entries = reaper.cache_snapshot();
        assert_eq!(entries.len(), 3, "Cache must stay at its cap");
        assert!(
            entries.iter().all(|e| e.id != b400.bundle.id),
            "Later-expiring bundle must be rejected by a full capped cache"
        );

        // The uncapped refill path bypasses the cap check entirely.
        reaper.watch(&b400, false);
        let entries = reaper.cache_snapshot();
        assert_eq!(entries.len(), 4, "Uncapped watch must grow the cache");
        assert!(
            entries.iter().any(|e| e.id == b400.bundle.id),
            "Uncapped watch must accept the later-expiring bundle"
        );
    }

    // watch() must wake the run loop exactly when the new entry becomes the
    // soonest expiry in the cache.
    #[tokio::test]
    async fn test_wakeup_trigger() {
        let reaper = make_reaper(8);

        let mut notified = core::pin::pin!(reaper.wakeup.notified());
        assert!(futures::poll!(notified.as_mut()).is_pending());

        // The cache was empty: the first entry must trigger a wakeup.
        reaper.watch(&make_bundle(200, 1), true);
        assert!(
            futures::poll!(notified.as_mut()).is_ready(),
            "First entry must trigger a wakeup"
        );

        let mut notified = core::pin::pin!(reaper.wakeup.notified());
        assert!(futures::poll!(notified.as_mut()).is_pending());

        // A sooner entry shortens the next deadline: must wake.
        reaper.watch(&make_bundle(100, 2), true);
        assert!(
            futures::poll!(notified.as_mut()).is_ready(),
            "Sooner entry must trigger a wakeup"
        );

        let mut notified = core::pin::pin!(reaper.wakeup.notified());
        assert!(futures::poll!(notified.as_mut()).is_pending());

        // A later entry leaves the deadline unchanged: no wakeup.
        reaper.watch(&make_bundle(300, 3), true);
        assert!(
            futures::poll!(notified.as_mut()).is_pending(),
            "Later entry must not trigger a wakeup"
        );
    }
}
