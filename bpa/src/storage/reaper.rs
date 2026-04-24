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
use hardy_async::sync::Mutex;
use hardy_async::{Notify, TaskPool};
use hardy_bpv7::bundle::Id;
use hardy_bpv7::eid::Eid;
use time::OffsetDateTime;
use tracing::{debug, error};

use crate::bundle::{Bundle, BundleStatus};
use crate::dispatcher::Dispatcher;
use crate::{Arc, BTreeSet};

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

    async fn refill_cache(&self) {
        let cancel_token = self.tasks.cancel_token().clone();
        let (tx, rx) = flume::bounded::<Bundle>(self.cache_size);

        join!(
            async {
                let _ = self
                    .metadata_storage
                    .poll_expiry(tx, self.cache_size)
                    .await
                    .inspect_err(|e| error!("Failed to poll store for expiry bundles: {e}"));
            },
            async {
                loop {
                    select_biased! {
                        bundle = rx.recv_async().fuse() => {
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

    #[test]
    fn test_cache_saturation() {
        let mut cache = BTreeSet::new();
        let cache_size = 3;

        let e100 = make_entry(100, 1);
        let e200 = make_entry(200, 2);
        let e300 = make_entry(300, 3);
        cache.insert(e100.clone());
        cache.insert(e200.clone());
        cache.insert(e300.clone());
        assert_eq!(cache.len(), cache_size);

        let e50 = make_entry(50, 4);
        if cache.len() >= cache_size {
            let last_expiry = cache.last().unwrap().expiry;
            if e50.expiry < last_expiry {
                cache.pop_last();
                cache.insert(e50.clone());
            }
        }

        assert_eq!(cache.len(), cache_size);
        assert_eq!(cache.first().unwrap().expiry, e50.expiry);
        assert!(!cache.contains(&e300));
    }

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

    #[test]
    fn test_wakeup_trigger() {
        let e200 = make_entry(200, 1);
        let e100 = make_entry(100, 2);
        let e300 = make_entry(300, 3);

        let old_expiry: Option<OffsetDateTime> = None;
        let needs_wakeup = match old_expiry {
            None => true,
            Some(old) => e200.expiry < old,
        };
        assert!(needs_wakeup, "First entry should trigger wakeup");

        let old_expiry = Some(e200.expiry);
        let needs_wakeup = match old_expiry {
            None => true,
            Some(old) => e100.expiry < old,
        };
        assert!(needs_wakeup, "Sooner entry should trigger wakeup");

        let needs_wakeup = match old_expiry {
            None => true,
            Some(old) => e300.expiry < old,
        };
        assert!(!needs_wakeup, "Later entry should not trigger wakeup");
    }
}
