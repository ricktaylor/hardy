use core::num::{NonZero, NonZeroUsize};
use flume::Sender;
use hardy_async::async_trait;
use hardy_async::sync::Mutex;
use hardy_bpv7::bundle::Id;
use hardy_bpv7::eid::Eid;
use lru::LruCache;
use tracing::info;

use super::{MetadataStorage, Result};
use crate::bundle::{Bundle, BundleMetadata, BundleStatus, Stored};

#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default, rename_all = "kebab-case"))]
pub struct Config {
    pub max_bundles: NonZeroUsize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_bundles: NonZero::new(1_048_576).unwrap(),
        }
    }
}

pub struct MetadataMemStorage {
    entries: Mutex<LruCache<Id, Option<Bundle<Stored>>>>,
}

impl MetadataMemStorage {
    pub fn new(config: &Config) -> Self {
        info!(
            "Using in-memory metadata storage (max {} bundles, non-persistent)",
            config.max_bundles
        );

        let entries = Mutex::new(LruCache::new(config.max_bundles));

        Self { entries }
    }

    /// Account for an entry leaving the LRU (eviction or replacement).
    fn on_remove(value: &Option<Bundle<Stored>>) {
        match value {
            Some(_) => metrics::gauge!("bpa.mem_metadata.entries").decrement(1.0),
            None => metrics::gauge!("bpa.mem_metadata.tombstones").decrement(1.0),
        }
    }

    /// Account for an entry entering the LRU.
    fn on_add(value: &Option<Bundle<Stored>>) {
        match value {
            Some(_) => metrics::gauge!("bpa.mem_metadata.entries").increment(1.0),
            None => metrics::gauge!("bpa.mem_metadata.tombstones").increment(1.0),
        }
    }

    /// Insert or replace a value in the LRU, updating metrics for all transitions:
    /// added, replaced, evicted.
    fn put(&self, key: Id, value: Option<Bundle<Stored>>) -> Result<()> {
        let prev = { self.entries.lock().put(key, value.clone()) };
        if let Some(prev) = prev {
            Self::on_remove(&prev);
        }
        Self::on_add(&value);
        Ok(())
    }

    /// Insert a new entry only if the key is absent. Returns false if already present.
    /// Accounts for LRU eviction of a different entry.
    fn push(&self, key: Id, value: Option<Bundle<Stored>>) -> Result<bool> {
        let evicted = {
            let mut entries = self.entries.lock();
            if entries.get(&key).is_some() {
                return Ok(false);
            }
            entries.push(key, value.clone()).map(|e| e.1)
        };

        if let Some(evicted) = evicted {
            Self::on_remove(&evicted);
        }
        Self::on_add(&value);
        Ok(true)
    }
}

#[async_trait]
impl MetadataStorage for MetadataMemStorage {
    async fn get(&self, bundle_id: &Id) -> Result<Option<Bundle<Stored>>> {
        Ok(self.entries.lock().peek(bundle_id).cloned().flatten())
    }

    async fn insert(&self, bundle: &Bundle<Stored>) -> Result<bool> {
        self.push(bundle.bundle.id.clone(), Some(bundle.clone()))
    }

    async fn replace(&self, bundle: &Bundle<Stored>) -> Result<()> {
        self.put(bundle.bundle.id.clone(), Some(bundle.clone()))
    }

    async fn update_status(&self, bundle: &Bundle<Stored>) -> Result<()> {
        self.replace(bundle).await
    }

    async fn tombstone(&self, bundle_id: &Id) -> Result<()> {
        self.put(bundle_id.clone(), None)
    }

    async fn mark_unconfirmed(&self) {
        // No-op for in-memory store
    }

    async fn confirm_exists(&self, _bundle_id: &Id) -> Result<Option<BundleMetadata>> {
        Ok(None)
    }

    async fn remove_unconfirmed(&self, _tx: Sender<Bundle<Stored>>) -> Result<()> {
        Ok(())
    }

    async fn reset_peer_queue(&self, peer: u32) -> Result<u64> {
        let mut updated = 0;
        for (_, v) in self.entries.lock().iter_mut() {
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

    async fn poll_expiry(&self, tx: Sender<Bundle<Stored>>, limit: usize) -> Result<()> {
        let mut entries: Vec<Bundle<Stored>> = self
            .entries
            .lock()
            .iter()
            .filter_map(|(_, v)| v.as_ref())
            .filter(|v| v.metadata.status != BundleStatus::New)
            .cloned()
            .collect();

        entries.sort_unstable_by_key(|b| b.expiry());

        for e in entries.into_iter().take(limit) {
            if tx.send_async(e).await.is_err() {
                break;
            }
        }
        Ok(())
    }

    async fn poll_waiting(&self, tx: Sender<Bundle<Stored>>) -> Result<()> {
        let mut entries: Vec<Bundle<Stored>> = self
            .entries
            .lock()
            .iter()
            .filter_map(|(_, v)| v.as_ref())
            .filter(|b| b.metadata.status == BundleStatus::Waiting)
            .cloned()
            .collect();

        entries.sort_unstable_by_key(|b| b.metadata.read_only.received_at);

        for bundle in entries {
            if tx.send_async(bundle).await.is_err() {
                break;
            }
        }
        Ok(())
    }

    async fn poll_service_waiting(&self, source: Eid, tx: Sender<Bundle<Stored>>) -> Result<()> {
        let mut entries: Vec<Bundle<Stored>> = self
            .entries
            .lock()
            .iter()
            .filter_map(|(_, v)| v.as_ref())
            .filter(|b| {
                matches!(&b.metadata.status, BundleStatus::WaitingForService { service } if service == &source)
            })
            .cloned()
            .collect();

        entries.sort_unstable_by_key(|b| b.metadata.read_only.received_at);

        for bundle in entries {
            if tx.send_async(bundle).await.is_err() {
                break;
            }
        }
        Ok(())
    }

    async fn poll_adu_fragments(
        &self,
        tx: Sender<Bundle<Stored>>,
        status: &BundleStatus,
    ) -> Result<()> {
        let mut entries: Vec<(u64, Bundle<Stored>)> = self
            .entries
            .lock()
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
            if tx.send_async(e).await.is_err() {
                break;
            }
        }
        Ok(())
    }

    async fn poll_pending(
        &self,
        tx: Sender<Bundle<Stored>>,
        status: &BundleStatus,
        limit: usize,
    ) -> Result<()> {
        let mut entries: Vec<Bundle<Stored>> = self
            .entries
            .lock()
            .iter()
            .filter_map(|(_, v)| v.as_ref())
            .filter(|v| &v.metadata.status == status)
            .cloned()
            .collect();

        entries.sort_unstable_by_key(|b| b.metadata.read_only.received_at);

        for e in entries.into_iter().take(limit) {
            if tx.send_async(e).await.is_err() {
                break;
            }
        }
        Ok(())
    }
}
