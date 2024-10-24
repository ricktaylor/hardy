use super::*;
use hardy_bpa_api::async_trait;
use rand::distributions::{Alphanumeric, DistString};
use std::{collections::HashMap, sync::Arc};
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
        match self.bundles.read().await.get(storage_name) {
            None => Ok(None),
            Some(v) => Ok(Some(Arc::new(DataRefWrapper(v.clone())))),
        }
    }

    async fn store(&self, data: Box<[u8]>) -> storage::Result<Arc<str>> {
        let mut data = Arc::from(data);
        let mut bundles = self.bundles.write().await;
        loop {
            let storage_name = Alphanumeric.sample_string(&mut rand::thread_rng(), 64);

            let Some(prev) = bundles.insert(storage_name.clone(), data) else {
                return Ok(storage_name.into());
            };

            // Swap back
            data = bundles.insert(storage_name, prev).unwrap();
        }
    }

    async fn remove(&self, storage_name: &str) -> storage::Result<()> {
        self.bundles.write().await.remove(storage_name);
        Ok(())
    }
}
