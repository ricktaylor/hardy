use super::*;
use metadata::*;
use std::collections::{HashMap, hash_map};
use thiserror::Error;
use tokio::sync::RwLock;

#[derive(Error, Debug)]
pub enum Error {
    #[error("No such bundle")]
    NotFound,
}

#[derive(Default)]
pub struct Storage {
    entries: RwLock<
        HashMap<hardy_bpv7::bundle::Id, (metadata::BundleMetadata, hardy_bpv7::bundle::Bundle)>,
    >,
}

#[async_trait]
impl storage::MetadataStorage for Storage {
    async fn load(
        &self,
        _bundle_id: &hardy_bpv7::bundle::Id,
    ) -> storage::Result<Option<(metadata::BundleMetadata, hardy_bpv7::bundle::Bundle)>> {
        todo!()
    }

    async fn store(
        &self,
        metadata: &BundleMetadata,
        bundle: &hardy_bpv7::bundle::Bundle,
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
        bundle_id: &hardy_bpv7::bundle::Id,
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
        bundle_id: &hardy_bpv7::bundle::Id,
        status: &BundleStatus,
    ) -> storage::Result<()> {
        self.entries
            .write()
            .await
            .get_mut(bundle_id)
            .map(|(m, _)| m.status = status.clone())
            .ok_or(Error::NotFound.into())
    }

    async fn remove(&self, bundle_id: &hardy_bpv7::bundle::Id) -> storage::Result<()> {
        self.entries
            .write()
            .await
            .remove(bundle_id)
            .map(|_| ())
            .ok_or(Error::NotFound.into())
    }

    async fn confirm_exists(
        &self,
        _bundle_id: &hardy_bpv7::bundle::Id,
    ) -> storage::Result<Option<BundleMetadata>> {
        Ok(None)
    }

    async fn get_unconfirmed_bundles(&self, _tx: storage::Sender) -> storage::Result<()> {
        // We have no persistence, so therefore no orphans
        Ok(())
    }
}
