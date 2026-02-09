use super::*;
use std::sync::Mutex;

#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct Config {
    #[cfg_attr(feature = "serde", serde(rename = "max-bundles"))]
    pub max_bundles: core::num::NonZeroUsize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_bundles: core::num::NonZero::new(1_048_576).unwrap(),
        }
    }
}

struct Storage {
    entries: Mutex<lru::LruCache<hardy_bpv7::bundle::Id, Option<bundle::Bundle>>>,
}

#[async_trait]
impl storage::MetadataStorage for Storage {
    async fn get(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> storage::Result<Option<bundle::Bundle>> {
        if let Some(bundle) = self
            .entries
            .lock()
            .trace_expect("Failed to lock mutex")
            .peek(bundle_id)
            .cloned()
        {
            Ok(bundle)
        } else {
            Ok(None)
        }
    }

    async fn insert(&self, bundle: &bundle::Bundle) -> storage::Result<bool> {
        let mut entries = self.entries.lock().trace_expect("Failed to lock mutex");
        if entries.get(&bundle.bundle.id).is_some() {
            Ok(false)
        } else {
            entries.put(bundle.bundle.id.clone(), Some(bundle.clone()));
            Ok(true)
        }
    }

    async fn replace(&self, bundle: &bundle::Bundle) -> storage::Result<()> {
        self.entries
            .lock()
            .trace_expect("Failed to lock mutex")
            .put(bundle.bundle.id.clone(), Some(bundle.clone()));
        Ok(())
    }

    async fn tombstone(&self, bundle_id: &hardy_bpv7::bundle::Id) -> storage::Result<()> {
        self.entries
            .lock()
            .trace_expect("Failed to lock mutex")
            .put(bundle_id.clone(), None);
        Ok(())
    }

    async fn start_recovery(&self) {
        // No-op for in-memory store
    }

    async fn confirm_exists(
        &self,
        _bundle_id: &hardy_bpv7::bundle::Id,
    ) -> storage::Result<Option<metadata::BundleMetadata>> {
        Ok(None)
    }

    async fn remove_unconfirmed(
        &self,
        _tx: storage::Sender<bundle::Bundle>,
    ) -> storage::Result<()> {
        Ok(())
    }

    async fn reset_peer_queue(&self, peer: u32) -> storage::Result<bool> {
        let mut updated = false;
        for (_, v) in self
            .entries
            .lock()
            .trace_expect("Failed to lock mutex")
            .iter_mut()
        {
            if let Some(v) = v
                && let metadata::BundleStatus::ForwardPending { peer: p, queue: _ } =
                    v.metadata.status
                && p == peer
            {
                v.metadata.status = metadata::BundleStatus::Waiting;
                updated = true;
            }
        }
        Ok(updated)
    }

    async fn poll_expiry(
        &self,
        tx: storage::Sender<bundle::Bundle>,
        mut limit: usize,
    ) -> storage::Result<()> {
        let mut entries = BTreeMap::new();
        for (_, v) in self
            .entries
            .lock()
            .trace_expect("Failed to lock mutex")
            .iter()
        {
            if let Some(v) = v
                && v.metadata.status != metadata::BundleStatus::New
            {
                entries.insert(v.expiry(), v.clone());
            }
        }

        for (_, e) in entries {
            if limit == 0 {
                break;
            }
            limit -= 1;

            if tx.send_async(e).await.is_err() {
                break;
            }
        }
        Ok(())
    }

    async fn poll_waiting(&self, tx: storage::Sender<bundle::Bundle>) -> storage::Result<()> {
        let mut entries = BTreeMap::new();
        for (_, v) in self
            .entries
            .lock()
            .trace_expect("Failed to lock mutex")
            .iter()
        {
            if let Some(v) = v
                && v.metadata.status == metadata::BundleStatus::Waiting
            {
                entries.insert(v.metadata.read_only.received_at, v.clone());
            }
        }

        for (_, e) in entries {
            if tx.send_async(e).await.is_err() {
                break;
            }
        }
        Ok(())
    }

    async fn poll_adu_fragments(
        &self,
        tx: storage::Sender<bundle::Bundle>,
        status: &metadata::BundleStatus,
    ) -> storage::Result<()> {
        let mut entries = BTreeMap::new();
        for (_, v) in self
            .entries
            .lock()
            .trace_expect("Failed to lock mutex")
            .iter()
        {
            if let Some(v) = v
                && &v.metadata.status == status
                && let Some(fi) = &v.bundle.id.fragment_info
            {
                entries.insert(fi.offset, v.clone());
            }
        }

        for (_, e) in entries {
            if tx.send_async(e).await.is_err() {
                break;
            }
        }
        Ok(())
    }

    async fn poll_pending(
        &self,
        tx: storage::Sender<bundle::Bundle>,
        state: &metadata::BundleStatus,
        mut limit: usize,
    ) -> storage::Result<()> {
        let mut entries = BTreeMap::new();
        for (_, v) in self
            .entries
            .lock()
            .trace_expect("Failed to lock mutex")
            .iter()
        {
            if let Some(v) = v
                && &v.metadata.status == state
            {
                entries.insert(v.metadata.read_only.received_at, v.clone());
            }
        }

        for (_, e) in entries {
            if limit == 0 {
                break;
            }
            limit -= 1;

            if tx.send_async(e).await.is_err() {
                break;
            }
        }
        Ok(())
    }
}

pub fn new(config: &Config) -> Arc<dyn storage::MetadataStorage> {
    Arc::new(Storage {
        entries: Mutex::new(lru::LruCache::new(config.max_bundles)),
    })
}
