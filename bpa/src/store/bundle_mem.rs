use super::*;
use hardy_bpa_api::async_trait;
use rand::distributions::{Alphanumeric, DistString};
use std::{
    collections::{hash_map, HashMap},
    sync::Arc,
};
use tokio::sync::RwLock;

pub const CONFIG_KEY: &str = "mem-storage";

struct DataRefWrapper(Arc<[u8]>);

impl AsRef<[u8]> for DataRefWrapper {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref()
    }
}

pub struct Storage {
    bundles: RwLock<HashMap<String, Arc<[u8]>>>,
}

impl Storage {
    #[instrument(skip_all)]
    pub fn init(_config: &HashMap<String, config::Value>) -> Arc<dyn storage::BundleStorage> {
        Arc::new(Self {
            bundles: RwLock::new(HashMap::new()),
        })
    }
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

    async fn load(&self, storage_name: &str) -> storage::Result<Option<storage::DataRef>> {
        if let Some(v) = self.bundles.read().await.get(storage_name) {
            Ok(Some(Arc::new(DataRefWrapper(v.clone()))))
        } else {
            Ok(None)
        }
    }

    async fn store(&self, data: &[u8]) -> storage::Result<Arc<str>> {
        let mut bundles = self.bundles.write().await;
        let mut rng = rand::thread_rng();
        loop {
            let storage_name = Alphanumeric.sample_string(&mut rng, 64);

            if let hash_map::Entry::Vacant(e) = bundles.entry(storage_name.clone()) {
                e.insert(Arc::from(data));
                return Ok(storage_name.into());
            }
        }
    }

    async fn remove(&self, storage_name: &str) -> storage::Result<()> {
        self.bundles.write().await.remove(storage_name);
        Ok(())
    }
}
