use super::*;
use std::sync::Arc;

pub struct Ingress<M, B>
where
    M: storage::MetadataStorage + Send + Sync + 'static,
    B: storage::BundleStorage + Send + Sync + 'static,
{
    cache: Arc<cache::Cache<M, B>>,
}

impl<M, B> Ingress<M, B>
where
    M: storage::MetadataStorage + Send + Sync + 'static,
    B: storage::BundleStorage + Send + Sync + 'static,
{
    pub fn new(
        _config: &config::Config,
        cache: Arc<cache::Cache<M, B>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            cache,
        })
    }

    pub async fn receive(&self, data: Arc<Vec<u8>>) -> Result<Option<String>, anyhow::Error> {
        // Store the bundle
        let Some(bundle) = self.cache.store(data).await? else {
            return Ok(Some("Unintelligible bundle".to_string()));
        };

        // Put bundle into RX queue
        todo!()
    }
}