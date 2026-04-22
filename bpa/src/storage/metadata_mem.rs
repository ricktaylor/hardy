use core::num::{NonZero, NonZeroUsize};

use hardy_async::async_trait;
use hardy_async::sync::Mutex;
use hardy_bpv7::bundle::Id;
use hardy_bpv7::creation_timestamp::CreationTimestamp;
use hardy_bpv7::eid::Eid;
use lru::LruCache;
use time::OffsetDateTime;
use tracing::info;

use super::{CompletionInfo, MetadataStorage, Result, Sender, UpsertResult};
use crate::bundle::{Bundle, BundleMetadata, BundleStatus};
use crate::fragmentation::{Coverage, FragmentDescriptor};
use crate::{Arc, Bytes, HashMap};

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

/// In-memory reassembly tracker entry.
struct ReassemblyEntry {
    storage_name: Option<Arc<str>>,
    total_adu_length: u64,
    coverage: Coverage,
    finalized: bool,
    extension_blocks: Option<Bytes>,
    expiry: OffsetDateTime,
}

pub struct MetadataMemStorage {
    entries: Mutex<LruCache<Id, Option<Bundle>>>,
    reassembly: Mutex<HashMap<(Eid, CreationTimestamp), ReassemblyEntry>>,
}

impl MetadataMemStorage {
    pub fn new(config: &Config) -> Self {
        info!(
            "Using in-memory metadata storage (max {} bundles, non-persistent)",
            config.max_bundles
        );

        let entries = Mutex::new(LruCache::new(config.max_bundles));

        Self {
            entries,
            reassembly: Mutex::new(HashMap::new()),
        }
    }

    /// Account for an entry leaving the LRU (eviction or replacement).
    fn on_remove(value: &Option<Bundle>) {
        match value {
            Some(_) => metrics::gauge!("bpa.mem_metadata.entries").decrement(1.0),
            None => metrics::gauge!("bpa.mem_metadata.tombstones").decrement(1.0),
        }
    }

    /// Account for an entry entering the LRU.
    fn on_add(value: &Option<Bundle>) {
        match value {
            Some(_) => metrics::gauge!("bpa.mem_metadata.entries").increment(1.0),
            None => metrics::gauge!("bpa.mem_metadata.tombstones").increment(1.0),
        }
    }

    /// Insert or replace a value in the LRU, updating metrics for all transitions:
    /// added, replaced, evicted.
    fn put(&self, key: Id, value: Option<Bundle>) -> Result<()> {
        let prev = { self.entries.lock().put(key, value.clone()) };
        if let Some(prev) = prev {
            Self::on_remove(&prev);
        }
        Self::on_add(&value);
        Ok(())
    }

    /// Insert a new entry only if the key is absent. Returns false if already present.
    /// Accounts for LRU eviction of a different entry.
    fn push(&self, key: Id, value: Option<Bundle>) -> Result<bool> {
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
    async fn get(&self, bundle_id: &Id) -> Result<Option<Bundle>> {
        Ok(self.entries.lock().peek(bundle_id).cloned().flatten())
    }

    async fn insert(&self, bundle: &Bundle) -> Result<bool> {
        self.push(bundle.bundle.id.clone(), Some(bundle.clone()))
    }

    async fn replace(&self, bundle: &Bundle) -> Result<()> {
        self.put(bundle.bundle.id.clone(), Some(bundle.clone()))
    }

    async fn update_status(&self, bundle: &Bundle) -> Result<()> {
        self.replace(bundle).await
    }

    async fn tombstone(&self, bundle_id: &Id) -> Result<()> {
        self.put(bundle_id.clone(), None)
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

    async fn remove_unconfirmed(&self, _tx: Sender<Bundle>) -> Result<()> {
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

    async fn poll_expiry(&self, tx: Sender<Bundle>, limit: usize) -> Result<()> {
        let mut entries: Vec<Bundle> = self
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

    async fn poll_waiting(&self, tx: Sender<Bundle>) -> Result<()> {
        let mut entries: Vec<Bundle> = self
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

    async fn poll_service_waiting(&self, source: Eid, tx: Sender<Bundle>) -> Result<()> {
        let mut entries: Vec<Bundle> = self
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

    async fn poll_adu_fragments(&self, tx: Sender<Bundle>, status: &BundleStatus) -> Result<()> {
        let mut entries: Vec<(u64, Bundle)> = self
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
        tx: Sender<Bundle>,
        state: &BundleStatus,
        limit: usize,
    ) -> Result<()> {
        let mut entries: Vec<Bundle> = self
            .entries
            .lock()
            .iter()
            .filter_map(|(_, v)| v.as_ref())
            .filter(|v| &v.metadata.status == state)
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

    async fn upsert_reassembly(&self, fragment: &FragmentDescriptor<'_>) -> Result<UpsertResult> {
        let key = (fragment.source.clone(), fragment.timestamp.clone());
        let mut map = self.reassembly.lock();

        let created = !map.contains_key(&key);
        let entry = map.entry(key).or_insert_with(|| ReassemblyEntry {
            storage_name: None,
            total_adu_length: fragment.total_adu_length,
            coverage: Coverage::new(),
            finalized: false,
            extension_blocks: None,
            expiry: fragment.expiry,
        });

        if entry.total_adu_length != fragment.total_adu_length {
            return Err(format!(
                "Fragment total_adu_length mismatch: expected {}, got {}",
                entry.total_adu_length, fragment.total_adu_length
            )
            .into());
        }

        // Only store extension blocks once (from fragment 0)
        if entry.extension_blocks.is_none() {
            if let Some(blocks) = fragment.extension_blocks {
                entry.extension_blocks = Some(blocks.clone());
            }
        }

        Ok(UpsertResult {
            storage_name: entry.storage_name.clone(),
            created,
        })
    }

    async fn confirm_fragment_write(
        &self,
        source: &Eid,
        timestamp: &CreationTimestamp,
        offset: u64,
        length: u64,
        total_adu_length: u64,
    ) -> Result<Option<CompletionInfo>> {
        let key = (source.clone(), timestamp.clone());
        let mut map = self.reassembly.lock();
        let entry = map.get_mut(&key).ok_or("Reassembly entry not found")?;

        entry.coverage.insert(offset, length);

        if !entry.finalized && entry.coverage.is_complete(total_adu_length) {
            entry.finalized = true;
            Ok(Some(CompletionInfo {
                extension_blocks: entry.extension_blocks.clone(),
            }))
        } else {
            Ok(None)
        }
    }

    async fn set_reassembly_name(
        &self,
        source: &Eid,
        timestamp: &CreationTimestamp,
        name: Arc<str>,
    ) -> Result<Arc<str>> {
        let mut map = self.reassembly.lock();
        let entry = map
            .get_mut(&(source.clone(), timestamp.clone()))
            .ok_or("Reassembly entry not found")?;

        // CAS: only set if currently None
        if entry.storage_name.is_none() {
            entry.storage_name = Some(name);
        }

        // Return the winning name (ours or the pre-existing one)
        Ok(entry.storage_name.clone().unwrap())
    }

    async fn delete_reassembly(&self, source: &Eid, timestamp: &CreationTimestamp) -> Result<()> {
        self.reassembly
            .lock()
            .remove(&(source.clone(), timestamp.clone()));
        Ok(())
    }

    async fn poll_expired_reassemblies(&self) -> Result<Vec<super::ExpiredReassembly>> {
        let now = time::OffsetDateTime::now_utc();
        let mut map = self.reassembly.lock();

        let expired_keys: Vec<_> = map
            .iter()
            .filter(|(_, entry)| entry.expiry <= now)
            .map(|(key, _)| key.clone())
            .collect();

        let mut result = Vec::with_capacity(expired_keys.len());
        for key in expired_keys {
            if let Some(entry) = map.remove(&key) {
                result.push(super::ExpiredReassembly {
                    source: key.0,
                    timestamp: key.1,
                    storage_name: entry.storage_name,
                });
            }
        }

        Ok(result)
    }
}
