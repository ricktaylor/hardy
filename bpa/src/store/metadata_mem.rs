use super::*;
use hardy_bpa_api::{async_trait, metadata};
use std::{
    collections::{hash_map, HashMap},
    sync::Arc,
};
use thiserror::Error;
use tokio::sync::RwLock;

pub const CONFIG_KEY: &str = "mem-storage";

#[derive(Error, Debug)]
pub enum Error {
    #[error("No such bundle")]
    NotFound,
}

pub struct Storage {
    entries: RwLock<HashMap<bpv7::BundleId, metadata::Bundle>>,
}

impl Storage {
    #[instrument(skip_all)]
    pub fn init(_config: &HashMap<String, config::Value>) -> Arc<dyn storage::MetadataStorage> {
        Arc::new(Self {
            entries: RwLock::new(HashMap::new()),
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
        if let hash_map::Entry::Vacant(e) = self.entries.write().await.entry(bundle.id.clone()) {
            e.insert(metadata::Bundle {
                metadata: metadata.clone(),
                bundle: bundle.clone(),
            });
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn get_bundle_status(
        &self,
        bundle_id: &bpv7::BundleId,
    ) -> storage::Result<Option<metadata::BundleStatus>> {
        Ok(self
            .entries
            .read()
            .await
            .get(bundle_id)
            .map(|bundle| bundle.metadata.status.clone()))
    }

    async fn set_bundle_status(
        &self,
        bundle_id: &bpv7::BundleId,
        status: &metadata::BundleStatus,
    ) -> storage::Result<()> {
        self.entries
            .write()
            .await
            .get_mut(bundle_id)
            .map(|bundle| bundle.metadata.status = status.clone())
            .ok_or(Error::NotFound.into())
    }

    async fn remove(&self, bundle_id: &bpv7::BundleId) -> storage::Result<()> {
        self.entries
            .write()
            .await
            .remove(bundle_id)
            .map(|_| ())
            .ok_or(Error::NotFound.into())
    }

    async fn confirm_exists(
        &self,
        _bundle_id: &bpv7::BundleId,
    ) -> storage::Result<Option<metadata::Metadata>> {
        Ok(None)
    }

    async fn get_waiting_bundles(
        &self,
        limit: time::OffsetDateTime,
        tx: storage::Sender,
    ) -> storage::Result<()> {
        // Drop all tombstones and collect waiting
        let mut tombstones = Vec::new();

        let mut entries = self.entries.write().await;

        for (bundle_id, bundle) in entries.iter() {
            match bundle.metadata.status {
                metadata::BundleStatus::Tombstone(from)
                    if from + time::Duration::seconds(5) < time::OffsetDateTime::now_utc() =>
                {
                    tombstones.push(bundle_id.clone());
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
        for bundle_id in tombstones {
            entries.remove(&bundle_id);
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
