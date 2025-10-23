use super::*;

// CacheEntry stores the expiry and ID for the heap.
#[derive(Clone, Eq, PartialEq)]
pub struct CacheEntry {
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

impl Store {
    /// Adds a bundle to the Reaper's cache to be monitored.
    /// If a new bundle has the soonest expiry, the background task is notified.
    pub async fn watch_bundle(&self, bundle: bundle::Bundle) {
        self.watch_bundle_inner(bundle, true).await;
    }

    async fn watch_bundle_inner(&self, bundle: bundle::Bundle, cap: bool) {
        let new_entry = CacheEntry {
            expiry: bundle.expiry(),
            id: bundle.bundle.id,
            destination: bundle.bundle.destination,
        };

        let new_expiry = new_entry.expiry;
        let old_expiry = {
            let mut cache = self
                .reaper_cache
                .lock()
                .trace_expect("Failed to acquire lock");
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

    /// The background task that waits for the next expiry, a notification, or shutdown.
    pub async fn run_reaper(self: Arc<Self>, dispatcher: Arc<dispatcher::Dispatcher>) {
        let mut repopulation_task: Option<tokio::task::JoinHandle<()>> = None;

        loop {
            let sleep_duration = {
                if let Some(entry) = self
                    .reaper_cache
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
                _ = self.reaper_wakeup.notified() => {},
                _ = self.cancel_token.cancelled() => {
                    // Shutting down
                    break;
                }
            }

            let mut dead_bundle_ids = Vec::new();
            let check_store = {
                let mut cache = self
                    .reaper_cache
                    .lock()
                    .trace_expect("Failed to acquire lock");

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
                    .metadata_storage
                    .get(&id)
                    .await
                    .inspect_err(|e| error!("Failed to get metadata from store: {e}"))
                {
                    dispatcher
                        .drop_bundle(
                            bundle,
                            Some(hardy_bpv7::status_report::ReasonCode::LifetimeExpired),
                        )
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
                let task = async move { reaper.refill_cache().await };

                #[cfg(feature = "tracing")]
                let task = {
                    let span = tracing::trace_span!(parent: None, "refill_cache_task");
                    span.follows_from(tracing::Span::current());
                    task.instrument(span)
                };

                repopulation_task = Some(self.task_tracker.spawn(task));
            }
        }
    }

    async fn refill_cache(self: Arc<Self>) {
        let outer_cancel_token = self.cancel_token.child_token();
        let cancel_token = outer_cancel_token.clone();
        let reaper = self.clone();
        let (tx, rx) = flume::bounded::<bundle::Bundle>(self.reaper_cache_size);
        let task = async move {
            loop {
                tokio::select! {
                    bundle = rx.recv_async() => {
                        let Ok(bundle) = bundle else {
                            break;
                        };
                        if bundle.metadata.status != metadata::BundleStatus::Dispatching {
                            reaper.watch_bundle_inner(bundle, false).await;
                        }
                    },
                    _ = cancel_token.cancelled() => {
                        break;
                    }
                }
            }
        };

        #[cfg(feature = "tracing")]
        let task = {
            let span = tracing::trace_span!(parent: None, "poll_expiry_reader");
            span.follows_from(tracing::Span::current());
            task.instrument(span)
        };

        let h = tokio::spawn(task);

        if self
            .metadata_storage
            .poll_expiry(tx, self.reaper_cache_size)
            .await
            .inspect_err(|e| error!("Failed to poll store for expiry bundles: {e}"))
            .is_err()
        {
            // Cancel the reader task
            outer_cancel_token.cancel();
        }

        _ = h.await;
    }
}
