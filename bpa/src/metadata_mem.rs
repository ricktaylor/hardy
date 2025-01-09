use super::*;
use metadata::*;
use std::collections::{hash_map, HashMap};
use thiserror::Error;
use tokio::sync::RwLock;

#[derive(Error, Debug)]
pub enum Error {
    #[error("No such bundle")]
    NotFound,
}

#[derive(Default)]
pub struct Storage {
    entries: RwLock<HashMap<bpv7::BundleId, (metadata::BundleMetadata, bpv7::Bundle)>>,
}

#[async_trait]
impl storage::MetadataStorage for Storage {
    async fn load(
        &self,
        _bundle_id: &bpv7::BundleId,
    ) -> storage::Result<Option<(metadata::BundleMetadata, bpv7::Bundle)>> {
        todo!()
    }

    async fn store(
        &self,
        metadata: &BundleMetadata,
        bundle: &bpv7::Bundle,
    ) -> storage::Result<bool> {
        if let hash_map::Entry::Vacant(e) = self.entries.write().await.entry(bundle.id.clone()) {
            e.insert((metadata.clone(), bundle.clone()));
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn get_bundle_status(
        &self,
        bundle_id: &bpv7::BundleId,
    ) -> storage::Result<Option<BundleStatus>> {
        Ok(self
            .entries
            .read()
            .await
            .get(bundle_id)
            .map(|(m, _)| m.status.clone()))
    }

    async fn set_bundle_status(
        &self,
        bundle_id: &bpv7::BundleId,
        status: &BundleStatus,
    ) -> storage::Result<()> {
        self.entries
            .write()
            .await
            .get_mut(bundle_id)
            .map(|(m, _)| m.status = status.clone())
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
    ) -> storage::Result<Option<BundleMetadata>> {
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

        for (bundle_id, (m, b)) in entries.iter() {
            match m.status {
                BundleStatus::Tombstone(from)
                    if from + time::Duration::seconds(5) < time::OffsetDateTime::now_utc() =>
                {
                    tombstones.push(bundle_id.clone());
                }
                BundleStatus::ForwardAckPending(_, until) | BundleStatus::Waiting(until)
                    if until <= limit =>
                {
                    if tx.send((m.clone(), b.clone())).await.is_err() {
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
        _destination: &bpv7::Eid,
        _tx: storage::Sender,
    ) -> storage::Result<()> {
        todo!()
    }
}
