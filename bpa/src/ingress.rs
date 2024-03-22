use super::*;
use std::sync::Arc;

pub struct Ingress<M, B>
where
    M: storage::MetadataStorage + Send + Sync,
    B: storage::BundleStorage + Send + Sync,
{
    cache: Arc<cache::Cache<M, B>>,
}

impl<M, B> Ingress<M, B>
where
    M: storage::MetadataStorage + Send + Sync,
    B: storage::BundleStorage + Send + Sync,
{
    fn new(cache: Arc<cache::Cache<M, B>>) -> Arc<Self> {
        Arc::new(Self { cache })
    }

    pub async fn receive(&self, data: Arc<Vec<u8>>) -> Result<Option<String>, anyhow::Error> {
        // Store the bundle in the cache
        let Some(bundle) = self.cache.store(data).await? else {
            return Ok(Some("Unintelligible bundle".to_string()));
        };

        // Put bundle into RX queue
        todo!()
    }
}

pub fn init<M, B>(
    config: &config::Config,
    cache: Arc<cache::Cache<M, B>>,
    task_set: &mut tokio::task::JoinSet<()>,
    cancel_token: tokio_util::sync::CancellationToken,
) -> Result<Arc<Ingress<M, B>>, anyhow::Error>
where
    M: storage::MetadataStorage + Send + Sync,
    B: storage::BundleStorage + Send + Sync,
{
    let ingress = Ingress::new(cache);
    Ok(ingress)
}
