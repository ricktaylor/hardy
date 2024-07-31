use super::*;
use hardy_bpa_api::{async_trait, metadata};
use std::{collections::HashMap, sync::Arc};
use thiserror::Error;
use tokio::sync::RwLock;

pub const CONFIG_KEY: &str = "mem-storage";

#[derive(Error, Debug)]
pub enum Error {
    #[error("No such bundle")]
    NotFound,
}

struct StorageInner {
    metadata: HashMap<String, metadata::Bundle>,
    index: HashMap<bpv7::BundleId, String>,
}

pub struct Storage {
    inner: RwLock<StorageInner>,
}

impl Storage {
    #[instrument(skip_all)]
    pub fn init(_config: &HashMap<String, config::Value>) -> Arc<dyn storage::MetadataStorage> {
        Arc::new(Self {
            inner: RwLock::new(StorageInner {
                metadata: HashMap::new(),
                index: HashMap::new(),
            }),
        })
    }
}

#[async_trait]
impl storage::MetadataStorage for Storage {
    fn get_unconfirmed_bundles(
        &self,
        _f: &mut dyn FnMut(metadata::Bundle) -> storage::Result<bool>,
    ) -> storage::Result<()> {
        // We have no persistence, so therefore no orphans
        Ok(())
    }

    fn restart(
        &self,
        _f: &mut dyn FnMut(metadata::Bundle) -> storage::Result<bool>,
    ) -> storage::Result<()> {
        // We have no persistence, so therefore nothing to restart
        Ok(())
    }

    async fn load(&self, _bundle_id: &bpv7::BundleId) -> storage::Result<Option<metadata::Bundle>> {
        todo!()
    }

    async fn store(
        &self,
        metadata: &metadata::Metadata,
        bundle: &bpv7::Bundle,
    ) -> storage::Result<bool> {
        let mut inner = self.inner.write().await;
        match inner
            .index
            .insert(bundle.id.clone(), metadata.storage_name.clone())
        {
            None => {}
            Some(prev) => {
                inner.index.insert(bundle.id.clone(), prev);
                return Ok(false);
            }
        }

        let Some(prev) = inner.metadata.insert(
            metadata.storage_name.clone(),
            metadata::Bundle {
                metadata: metadata.clone(),
                bundle: bundle.clone(),
            },
        ) else {
            return Ok(true);
        };

        // Swap back
        inner.metadata.insert(metadata.storage_name.clone(), prev);
        Ok(false)
    }

    async fn get_bundle_status(
        &self,
        storage_name: &str,
    ) -> storage::Result<Option<metadata::BundleStatus>> {
        Ok(self
            .inner
            .read()
            .await
            .metadata
            .get(storage_name)
            .map(|m| m.metadata.status.clone()))
    }

    async fn set_bundle_status(
        &self,
        storage_name: &str,
        status: &metadata::BundleStatus,
    ) -> storage::Result<()> {
        self.inner
            .write()
            .await
            .metadata
            .get_mut(storage_name)
            .map(|m| m.metadata.status = status.clone())
            .ok_or(Error::NotFound.into())
    }

    async fn remove(&self, _storage_name: &str) -> storage::Result<()> {
        todo!()
    }

    async fn confirm_exists(&self, _storage_name: &str, _hash: &[u8]) -> storage::Result<bool> {
        todo!()
    }

    async fn begin_replace(&self, storage_name: &str, hash: &[u8]) -> storage::Result<()> {
        // We don't really have anything transactional to do here, just confirm the bundle exists
        self.confirm_exists(storage_name, hash)
            .await
            .map(|exists| exists.then_some(()))?
            .ok_or(Error::NotFound.into())
    }

    async fn commit_replace(&self, _storage_name: &str, _hash: &[u8]) -> storage::Result<()> {
        todo!()
    }

    async fn get_waiting_bundles(
        &self,
        limit: time::OffsetDateTime,
    ) -> storage::Result<Vec<(metadata::Bundle, time::OffsetDateTime)>> {
        // Drop all tombstones and collect waiting
        let mut tombstones = Vec::new();
        let mut waiting = Vec::new();

        let mut inner = self.inner.write().await;

        inner.metadata.retain(|_, bundle| {
            match bundle.metadata.status {
                metadata::BundleStatus::Tombstone(from)
                    if from + time::Duration::seconds(5) < time::OffsetDateTime::now_utc() =>
                {
                    tombstones.push(bundle.bundle.id.clone());
                    return false;
                }
                metadata::BundleStatus::Waiting(until) if until <= limit => {
                    waiting.push((bundle.clone(), until));
                }
                _ => {}
            }
            true
        });

        // Remove tombstones from index
        for b in tombstones {
            inner.index.remove(&b);
        }

        Ok(waiting)
    }

    async fn poll_for_collection(
        &self,
        _destination: bpv7::Eid,
    ) -> storage::Result<Vec<metadata::Bundle>> {
        todo!()
    }
}
