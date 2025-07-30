use super::*;
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
    entries: RwLock<HashMap<hardy_bpv7::bundle::Id, bundle::Bundle>>,
}

#[async_trait]
impl storage::MetadataStorage for Storage {
    async fn load(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> storage::Result<Option<bundle::Bundle>> {
        Ok(self.entries.read().await.get(bundle_id).cloned())
    }

    async fn store(&self, bundle: &bundle::Bundle) -> storage::Result<bool> {
        if let hash_map::Entry::Vacant(e) =
            self.entries.write().await.entry(bundle.bundle.id.clone())
        {
            e.insert(bundle.clone());
            Ok(true)
        } else {
            Ok(false)
        }
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
    ) -> storage::Result<Option<metadata::BundleMetadata>> {
        Ok(None)
    }

    async fn remove_unconfirmed_bundles(&self, _tx: storage::Sender) -> storage::Result<()> {
        // We have no persistence, so therefore no orphans
        Ok(())
    }
}
