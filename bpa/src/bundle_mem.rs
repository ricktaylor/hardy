use super::*;
use rand::distr::{Alphanumeric, SampleString};
use std::{
    collections::{HashMap, hash_map},
    sync::Arc,
};
use tokio::sync::RwLock;

#[derive(Default)]
pub struct Storage {
    bundles: RwLock<HashMap<String, Bytes>>,
}

#[async_trait]
impl storage::BundleStorage for Storage {
    async fn list(
        &self,
        _tx: tokio::sync::mpsc::Sender<storage::ListResponse>,
    ) -> storage::Result<()> {
        // We have no persistence, so therefore no bundles
        Ok(())
    }

    async fn load(&self, storage_name: &str) -> storage::Result<Option<Bytes>> {
        if let Some(v) = self.bundles.read().await.get(storage_name).cloned() {
            Ok(Some(Bytes::from_owner(v.clone())))
        } else {
            Ok(None)
        }
    }

    async fn store(&self, data: Bytes) -> storage::Result<Arc<str>> {
        let mut bundles = self.bundles.write().await;
        let mut rng = rand::rng();
        loop {
            let storage_name = Alphanumeric.sample_string(&mut rng, 64);

            if let hash_map::Entry::Vacant(e) = bundles.entry(storage_name.clone()) {
                e.insert(data);
                return Ok(storage_name.into());
            }
        }
    }

    async fn remove(&self, storage_name: &str) -> storage::Result<()> {
        self.bundles.write().await.remove(storage_name);
        Ok(())
    }
}
