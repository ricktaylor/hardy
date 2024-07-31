use super::*;
use hardy_bpa_api::async_trait;
use rand::distributions::{Alphanumeric, DistString};
use std::{collections::HashMap, sync::Arc};
use thiserror::Error;
use tokio::sync::RwLock;

pub const CONFIG_KEY: &str = "mem-storage";

#[derive(Error, Debug)]
pub enum Error {
    #[error("No such bundle")]
    NotFound,
}

pub struct Storage {
    bundles: RwLock<HashMap<String, Arc<Vec<u8>>>>,
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
    #[allow(clippy::type_complexity)]
    fn list(
        &self,
        _f: &mut dyn FnMut(&str, &[u8], Option<time::OffsetDateTime>) -> storage::Result<bool>,
    ) -> storage::Result<()> {
        // We have no persistence, so therefore no orphans
        Ok(())
    }

    async fn load(&self, storage_name: &str) -> storage::Result<storage::DataRef> {
        match self.bundles.read().await.get(storage_name) {
            None => Err(Error::NotFound.into()),
            Some(v) => Ok(v.clone()),
        }
    }

    async fn store(&self, data: Vec<u8>) -> storage::Result<String> {
        let mut data = Arc::new(data);
        let mut bundles = self.bundles.write().await;

        loop {
            let storage_name = Alphanumeric.sample_string(&mut rand::thread_rng(), 64);

            let Some(prev) = bundles.insert(storage_name.clone(), data) else {
                return Ok(storage_name);
            };

            // Swap back
            data = bundles.insert(storage_name, prev).unwrap();
        }
    }

    async fn remove(&self, storage_name: &str) -> storage::Result<()> {
        self.bundles
            .write()
            .await
            .remove(storage_name)
            .map(|_| ())
            .ok_or(Error::NotFound.into())
    }

    async fn replace(&self, _storage_name: &str, _data: Vec<u8>) -> storage::Result<()> {
        todo!()
    }
}
