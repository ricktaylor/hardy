use hardy_async::{async_trait, sync::Mutex};
use hardy_bpv7::{bundle::Id, eid::Eid};
use lru::LruCache;
use tracing::info;

use super::{MetadataStorage, Result, Sender};
use crate::bundle::{Bundle, BundleMetadata, BundleStatus};

#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default, rename_all = "kebab-case"))]
pub struct Config {
    pub max_bundles: core::num::NonZeroUsize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_bundles: core::num::NonZero::new(1_048_576).unwrap(),
        }
    }
}

pub struct MetadataMemStorage {
    entries: Mutex<LruCache<Id, Option<Bundle>>>,
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
}

#[async_trait]
impl MetadataStorage for MetadataMemStorage {
    async fn get(&self, bundle_id: &Id) -> Result<Option<Bundle>> {
        Ok(self.entries.lock().peek(bundle_id).cloned().flatten())
    }

    async fn insert(&self, bundle: &Bundle) -> Result<bool> {
        let mut entries = self.entries.lock();
        if entries.peek(&bundle.bundle.id).is_some() {
            Ok(false)
        } else {
            entries.put(bundle.bundle.id.clone(), Some(bundle.clone()));
            Ok(true)
        }
    }

    async fn replace(&self, bundle: &Bundle) -> Result<()> {
        self.entries
            .lock()
            .put(bundle.bundle.id.clone(), Some(bundle.clone()));
        Ok(())
    }

    async fn update_status(&self, bundle: &Bundle) -> Result<()> {
        self.replace(bundle).await
    }

    async fn tombstone(&self, bundle_id: &Id) -> Result<()> {
        self.entries.lock().put(bundle_id.clone(), None);
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

    async fn remove_unconfirmed(&self, _tx: Sender<Bundle>) -> Result<()> {
        Ok(())
    }

    async fn reset_peer_queue(&self, peer: u32) -> Result<bool> {
        let mut updated = false;
        for (_, v) in self.entries.lock().iter_mut() {
            if let Some(v) = v
                && let BundleStatus::ForwardPending { peer: p, queue: _ } = v.metadata.status
                && p == peer
            {
                v.metadata.status = BundleStatus::Waiting;
                updated = true;
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
}
