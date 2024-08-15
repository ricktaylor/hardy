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
    metadata: HashMap<Arc<str>, metadata::Bundle>,
    index: HashMap<bpv7::BundleId, Arc<str>>,
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

    async fn remove(&self, storage_name: &str) -> storage::Result<()> {
        let mut inner = self.inner.write().await;
        let Some(bundle) = inner.metadata.remove(storage_name) else {
            return Err(Error::NotFound.into());
        };

        inner
            .index
            .remove(&bundle.bundle.id)
            .map(|_| ())
            .ok_or(Error::NotFound.into())
    }

    async fn confirm_exists(
        &self,
        _bundle_id: &bpv7::BundleId,
    ) -> storage::Result<Option<metadata::Metadata>> {
        Ok(None)
    }

    async fn begin_replace(&self, _storage_name: &str, _hash: &[u8]) -> storage::Result<()> {
        todo!()
    }

    async fn commit_replace(&self, _storage_name: &str, _hash: &[u8]) -> storage::Result<()> {
        todo!()
    }

    async fn get_waiting_bundles(
        &self,
        limit: time::OffsetDateTime,
        tx: storage::Sender,
    ) -> storage::Result<()> {
        // Drop all tombstones and collect waiting
        let mut tombstones = Vec::new();

        let mut inner = self.inner.write().await;

        for bundle in inner.metadata.values() {
            match bundle.metadata.status {
                metadata::BundleStatus::Tombstone(from)
                    if from + time::Duration::seconds(5) < time::OffsetDateTime::now_utc() =>
                {
                    tombstones.push((
                        bundle.metadata.storage_name.clone(),
                        bundle.bundle.id.clone(),
                    ));
                }
                metadata::BundleStatus::ForwardAckPending(_, until)
                | metadata::BundleStatus::Waiting(until)
                    if until <= limit =>
                {
                    if tx.send(bundle.clone()).await.is_err() {
                        break;
                    }
                }
                _ => {}
            }
        }

        // Remove tombstones from index
        for (storage_name, id) in tombstones {
            inner.metadata.remove(&storage_name);
            inner.index.remove(&id);
        }
        Ok(())
    }

    async fn get_unconfirmed_bundles(&self, _tx: storage::Sender) -> storage::Result<()> {
        // We have no persistence, so therefore no orphans
        Ok(())
    }

    async fn poll_for_collection(
        &self,
        _destination: bpv7::Eid,
        _tx: storage::Sender,
    ) -> storage::Result<()> {
        todo!()
    }
}
