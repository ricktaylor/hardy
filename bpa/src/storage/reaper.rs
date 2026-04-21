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
use hardy_async::JoinHandle;
use hardy_async::time::sleep;
use hardy_bpv7::bundle::Id;
use hardy_bpv7::eid::Eid;
use hardy_bpv7::status_report::ReasonCode;
use time::OffsetDateTime;
use tracing::{debug, error};

use super::store::Store;
use crate::Arc;
use crate::bundle::{Bundle, BundleStatus};
use crate::dispatcher::Dispatcher;

/// Cache entry for the reaper's expiry monitoring.
///
/// Ordered by: expiry time → destination → bundle ID (for deterministic
/// BTreeSet ordering when expiry times collide).
#[derive(Clone, Eq, PartialEq)]
pub struct CacheEntry {
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

impl Store {
    /// Adds a bundle to the Reaper's cache to be monitored.
    /// If a new bundle has the soonest expiry, the background task is notified.
    pub async fn watch_bundle(&self, bundle: Bundle) {
        self.watch_bundle_inner(bundle, true).await;
    }

    async fn watch_bundle_inner(&self, bundle: Bundle, cap: bool) {
        let new_entry = CacheEntry {
            expiry: bundle.expiry(),
            id: bundle.bundle.id,
            destination: bundle.bundle.destination,
        };

        let new_expiry = new_entry.expiry;
        let old_expiry = {
            let mut cache = self.reaper_cache.lock();
            let old_expiry = cache.first().map(|e| e.expiry);

            if !cap || cache.len() < self.reaper_cache_size {
                // Case 1: Cache is not full, just insert.
                if !cache.insert(new_entry) {
                    // Just in case we have duplicates
                    return;
                }
            } else {
                // Case 2: Cache is full, check for eviction.
                let last_expiry = cache.last().map(|e| e.expiry).unwrap(); // Should always exist
                if new_expiry < last_expiry {
                    // New entry is better than the worst entry, so evict and insert.
                    cache.pop_last();
                    if !cache.insert(new_entry) {
                        // Just in case we have duplicates
                        return;
                    }
                } else {
                    // New entry is worse than the worst, so it's dropped.
                    return;
                }
            }
            old_expiry
        };

        let needs_wakeup = match old_expiry {
            None => true,
            Some(old_expiry) => new_expiry < old_expiry,
        };

        if needs_wakeup {
            self.reaper_wakeup.notify_one();
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
    pub async fn run_reaper(self: Arc<Self>, dispatcher: Arc<Dispatcher>) {
        let mut repopulation_task: Option<JoinHandle<()>> = None;

        loop {
            let sleep_duration = self
                .reaper_cache
                .lock()
                .first()
                .map(|entry| entry.expiry - OffsetDateTime::now_utc())
                .unwrap_or(time::Duration::MAX);

            select_biased! {
                _ = self.tasks.cancel_token().cancelled().fuse() => {
                    // Shutting down
                    debug!("Reaper task complete");
                    break;
                }
                _ = self.reaper_wakeup.notified().fuse() => {},
                _ = sleep(sleep_duration).fuse() => {},
            }

            let mut dead_bundle_ids = Vec::new();
            let check_store = {
                let mut cache = self.reaper_cache.lock();

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
                        .drop_bundle(bundle, Some(ReasonCode::LifetimeExpired))
                        .await;
                }
            }

            if check_store {
                // Check the local variable instead of a field on `self`.
                if let Some(handle) = &repopulation_task
                    && !handle.is_finished()
                {
                    // A task is active, so we do nothing.
                    continue; // Continue to the next loop iteration
                }

                // No active task, so we can spawn a new one.
                let reaper = self.clone();
                repopulation_task = Some(hardy_async::spawn!(
                    self.tasks,
                    "refill_cache_task",
                    async move { reaper.refill_cache().await }
                ));
            }
        }
    }

    async fn refill_cache(self: Arc<Self>) {
        let cancel_token = self.tasks.cancel_token().clone();
        let reaper = self.clone();
        let (tx, rx) = flume::bounded::<Bundle>(self.reaper_cache_size);

        join!(
            // Producer: poll for expiring bundles
            async {
                let _ = self
                    .metadata_storage
                    .poll_expiry(tx, self.reaper_cache_size)
                    .await
                    .inspect_err(|e| error!("Failed to poll store for expiry bundles: {e}"));
                // When tx is dropped, consumer will see channel close and exit
            },
            // Consumer: add bundles to cache
            async {
                loop {
                    select_biased! {
                        bundle = rx.recv_async().fuse() => {
                            let Ok(bundle) = bundle else {
                                break;
                            };
                            if bundle.metadata.status != BundleStatus::New {
                                reaper.watch_bundle_inner(bundle, false).await;
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
    use alloc::collections::BTreeSet;

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

    // CacheEntry BTreeSet should sort by expiry time (soonest first).
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

    // When cache is full, inserting a sooner entry should evict the latest.
    #[test]
    fn test_cache_saturation() {
        let mut cache = BTreeSet::new();
        let cache_size = 3;

        // Fill cache with entries at 100, 200, 300 seconds
        let e100 = make_entry(100, 1);
        let e200 = make_entry(200, 2);
        let e300 = make_entry(300, 3);
        cache.insert(e100.clone());
        cache.insert(e200.clone());
        cache.insert(e300.clone());
        assert_eq!(cache.len(), cache_size);

        // Insert sooner entry (50s) — should evict the latest (300s)
        let e50 = make_entry(50, 4);
        if cache.len() >= cache_size {
            let last_expiry = cache.last().unwrap().expiry;
            if e50.expiry < last_expiry {
                cache.pop_last();
                cache.insert(e50.clone());
            }
        }

        assert_eq!(cache.len(), cache_size);
        // Soonest should be e50
        assert_eq!(cache.first().unwrap().expiry, e50.expiry);
        // e300 should have been evicted
        assert!(!cache.contains(&e300));
    }

    // When cache is full, an entry with later expiry than the worst should be rejected.
    #[test]
    fn test_cache_rejection() {
        let mut cache = BTreeSet::new();
        let cache_size = 3;

        let e100 = make_entry(100, 1);
        let e200 = make_entry(200, 2);
        let e300 = make_entry(300, 3);
        cache.insert(e100.clone());
        cache.insert(e200.clone());
        cache.insert(e300.clone());

        // Try to insert an entry at 400s — later than worst (300s), should be rejected
        let e400 = make_entry(400, 4);
        let inserted = if cache.len() >= cache_size {
            let last_expiry = cache.last().unwrap().expiry;
            if e400.expiry < last_expiry {
                cache.pop_last();
                cache.insert(e400.clone());
                true
            } else {
                false
            }
        } else {
            cache.insert(e400.clone());
            true
        };

        assert!(!inserted);
        assert_eq!(cache.len(), cache_size);
        assert!(!cache.contains(&e400));
    }

    // Wakeup should trigger when a newly inserted entry is sooner than the current soonest.
    #[test]
    fn test_wakeup_trigger() {
        let e200 = make_entry(200, 1);
        let e100 = make_entry(100, 2);
        let e300 = make_entry(300, 3);

        // Simulate the wakeup logic from watch_bundle_inner
        let old_expiry: Option<OffsetDateTime> = None;

        // First entry into empty cache — should trigger wakeup
        let needs_wakeup = match old_expiry {
            None => true,
            Some(old) => e200.expiry < old,
        };
        assert!(needs_wakeup, "First entry should trigger wakeup");

        // Entry sooner than current soonest — should trigger
        let old_expiry = Some(e200.expiry);
        let needs_wakeup = match old_expiry {
            None => true,
            Some(old) => e100.expiry < old,
        };
        assert!(needs_wakeup, "Sooner entry should trigger wakeup");

        // Entry later than current soonest — should NOT trigger
        let needs_wakeup = match old_expiry {
            None => true,
            Some(old) => e300.expiry < old,
        };
        assert!(!needs_wakeup, "Later entry should not trigger wakeup");
    }
}
