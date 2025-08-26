use super::*;
use hardy_eid_pattern::{EidPattern, EidPatternSet};
use std::collections::BTreeSet;
use std::sync::Mutex;
use tokio::sync::Notify;

const CACHE_SIZE: usize = 64;

// CacheEntry stores the expiry and ID for the heap.
#[derive(Clone, Eq, PartialEq)]
struct CacheEntry {
    expiry: time::OffsetDateTime,
    id: hardy_bpv7::bundle::Id,
    destination: hardy_bpv7::eid::Eid,
}

impl PartialOrd for CacheEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for CacheEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.expiry
            .cmp(&other.expiry)
            .then_with(|| self.destination.cmp(&other.destination))
            .then_with(|| self.id.cmp(&other.id))
    }
}

/// A background component that monitors time-sensitive bundles and triggers
/// a cleanup action when they expire.
pub struct Sentinel {
    cancel_token: tokio_util::sync::CancellationToken,
    task_tracker: tokio_util::task::TaskTracker,
    cache: Arc<Mutex<BTreeSet<CacheEntry>>>,
    store: Arc<store::Store>,
    max_cache_size: usize,
    wakeup: Arc<Notify>,
    route_updates_tx: flume::Sender<EidPattern>,
}

impl Sentinel {
    /// Creates a new Sentinel. Returns the instance and the receiver for route updates.
    pub fn new(store: Arc<store::Store>) -> (Self, flume::Receiver<EidPattern>) {
        let cache = Arc::new(Mutex::new(BTreeSet::new()));
        let wakeup = Arc::new(Notify::new());
        let cancel_token = tokio_util::sync::CancellationToken::new();
        let task_tracker = tokio_util::task::TaskTracker::new();

        // Use flume::bounded for a channel with a fixed capacity.
        let (route_updates_tx, route_updates_rx) = flume::bounded(16);

        (
            Self {
                cache,
                max_cache_size: CACHE_SIZE,
                task_tracker,
                store,
                wakeup,
                cancel_token,
                route_updates_tx,
            },
            route_updates_rx,
        )
    }

    pub fn start(
        self: &Arc<Self>,
        dispatcher: Arc<dispatcher::Dispatcher>,
        route_rx: flume::Receiver<EidPattern>,
    ) {
        let sentinel = self.clone();
        let span = tracing::trace_span!("parent: None", "sentinel_task");
        span.follows_from(tracing::Span::current());
        self.task_tracker
            .spawn(async move { sentinel.run(dispatcher, route_rx).await }.instrument(span));
    }

    /// Provides the public API for other components to signal a route change.
    pub async fn new_route(&self, pattern: EidPattern) {
        // Use send_async for the async version of flume's send.
        _ = self.route_updates_tx.send_async(pattern).await;
    }

    /// Adds a bundle to the Sentinel's cache to be monitored.
    /// If a new bundle has the soonest expiry, the background task is notified.
    pub async fn watch_bundle(&self, bundle: bundle::Bundle) -> bool {
        let new_entry = CacheEntry {
            expiry: bundle.expiry(),
            id: bundle.bundle.id,
            destination: bundle.bundle.destination,
        };

        let new_expiry = new_entry.expiry;
        let old_expiry = {
            let mut cache = self.cache.lock().trace_expect("Failed to acquire lock");
            let old_expiry = cache.first().map(|e| e.expiry);

            if cache.len() < self.max_cache_size {
                // Case 1: Cache is not full, just insert.
                if !cache.insert(new_entry) {
                    // Just in case we have duplicates
                    return true;
                }
            } else {
                // Case 2: Cache is full, check for eviction.
                let last_expiry = cache.last().map(|e| e.expiry).unwrap(); // Should always exist
                if new_expiry < last_expiry {
                    // New entry is better than the worst entry, so evict and insert.
                    cache.pop_last();
                    if !cache.insert(new_entry) {
                        // Just in case we have duplicates
                        return true;
                    }
                } else {
                    // New entry is worse than the worst, so it's dropped.
                    return false;
                }
            }
            old_expiry
        };

        let needs_wakeup = match old_expiry {
            None => true,
            Some(old_expiry) => new_expiry < old_expiry,
        };

        if needs_wakeup {
            self.wakeup.notify_one();
        }

        true
    }

    /// The background task that waits for the next expiry, a notification, or shutdown.
    async fn run(
        self: Arc<Self>,
        dispatcher: Arc<dispatcher::Dispatcher>,
        route_rx: flume::Receiver<EidPattern>,
    ) {
        let mut repopulation_task: Option<tokio::task::JoinHandle<()>> = None;

        loop {
            let sleep_duration = {
                if let Some(entry) = self
                    .cache
                    .lock()
                    .trace_expect("Failed to acquire lock")
                    .first()
                {
                    // Calculate precise duration until the next expiry.
                    let sleep_duration = entry.expiry - time::OffsetDateTime::now_utc();
                    if sleep_duration.is_positive() {
                        sleep_duration
                            .try_into()
                            .unwrap_or(std::time::Duration::MAX)
                    } else {
                        std::time::Duration::ZERO
                    }
                } else {
                    // Cache is empty. Wait "forever" for a notification.
                    std::time::Duration::MAX
                }
            };

            tokio::select! {
                _ = tokio::time::sleep(sleep_duration) => {},
                _ = self.wakeup.notified() => {},

                // Use recv_async to await a message from the flume channel.
                Ok(pattern) = route_rx.recv_async() => {
                    info!("Handling new route pattern: {pattern}");
                    self.handle_new_route(&dispatcher, pattern.into()).await;
                    continue;
                },

                _ = self.cancel_token.cancelled() => {
                    // Shutting down
                    break;
                }
            }

            let mut dead_bundle_ids = Vec::new();
            let check_store = {
                let mut cache = self.cache.lock().trace_expect("Failed to acquire lock");

                let now = time::OffsetDateTime::now_utc();
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
                    .store
                    .get_metadata(&id)
                    .await
                    .inspect_err(|e| error!("Failed to get metadata from store: {e}"))
                {
                    _ = dispatcher
                        .drop_bundle(
                            bundle,
                            Some(hardy_bpv7::status_report::ReasonCode::LifetimeExpired),
                        )
                        .await
                        .inspect_err(|e| error!("Failed to drop expired bundle: {e}"));
                }
            }

            if check_store {
                // Check the local variable instead of a field on `self`.
                if let Some(handle) = &repopulation_task {
                    if !handle.is_finished() {
                        // A task is active, so we do nothing.
                        continue; // Continue to the next loop iteration
                    }
                }

                // No active task, so we can spawn a new one.
                info!("Cache empty, spawning task to repopulate from store.");

                let sentinel = self.clone();
                let span = tracing::trace_span!("parent: None", "refill_cache_task");
                span.follows_from(tracing::Span::current());
                repopulation_task = Some(
                    self.task_tracker
                        .spawn(async move { sentinel.refill_cache().await }.instrument(span)),
                );
            }
        }
    }

    /// Handles the logic for a new route announcement.
    ///
    /// This involves two steps:
    /// 1. An immediate, fast check of the in-memory cache.
    /// 2. Triggering a slow, background check of the store.
    async fn handle_new_route(
        self: &Arc<Self>,
        dispatcher: &Arc<dispatcher::Dispatcher>,
        filter: EidPatternSet,
    ) {
        // --- Action 1: Immediate Cache Check ---
        let bundles_to_forward = {
            let mut cache = self.cache.lock().expect("Failed to acquire lock");

            // .retain() iterates through the cache and keeps only the elements
            // for which the closure returns `true`.
            let mut ids = Vec::new();
            cache.retain(|entry| {
                if filter.contains(&entry.destination) {
                    // This bundle matches the new route.
                    ids.push(entry.id.clone());
                    false // Return `false` to REMOVE the item from the cache.
                } else {
                    true // Return `true` to KEEP the item in the cache.
                }
            });
            ids
        };

        // With the lock released, perform the dispatching
        for id in bundles_to_forward {
            if let Ok(Some(bundle)) = self
                .store
                .get_metadata(&id)
                .await
                .inspect_err(|e| error!("Failed to get metadata from store: {e}"))
            {
                // Forward the bundle
                _ = dispatcher
                    .forward_bundle(bundle)
                    .await
                    .inspect_err(|e| error!("Failed to dispatch bundle: {e}"));
            }
        }

        // --- Action 2: Trigger Slow Background Store Check ---
        let sentinel = self.clone();
        let dispatcher = dispatcher.clone();
        let span = tracing::trace_span!("parent: None", "search_store_task");
        span.follows_from(tracing::Span::current());
        self.task_tracker
            .spawn(async move { sentinel.search_store(dispatcher, filter).await }.instrument(span));
    }

    pub async fn shutdown(&self) {
        self.cancel_token.cancel();
        self.task_tracker.close();
        self.task_tracker.wait().await;
    }

    async fn refill_cache(self: Arc<Self>) {
        let outer_cancel_token = self.cancel_token.child_token();
        let cancel_token = outer_cancel_token.clone();
        let sentinel = self.clone();
        let span = tracing::trace_span!("parent: None", "refill_cache_reader");
        span.follows_from(tracing::Span::current());
        let (tx, rx) = flume::bounded::<bundle::Bundle>(self.max_cache_size);
        let h = tokio::spawn(
            async move {
                loop {
                    tokio::select! {
                        bundle = rx.recv_async() => match bundle {
                            Err(_) => break,
                            Ok(bundle) => if !sentinel.watch_bundle(bundle).await {
                                // Cache is now full
                                break;
                            }
                        },
                        _ = cancel_token.cancelled() => {
                            break;
                        }
                    }
                }
            }
            .instrument(span),
        );

        if self
            .store
            .poll_pending(tx)
            .await
            .inspect_err(|e| error!("Failed to poll store for pending bundles: {e}"))
            .is_ok()
        {
            // Cancel the reader task
            outer_cancel_token.cancel();
        }

        _ = h.await;
    }

    async fn search_store(
        self: Arc<Self>,
        _dispatcher: Arc<dispatcher::Dispatcher>,
        _filter: EidPatternSet,
    ) {
        //todo!();
    }
}
