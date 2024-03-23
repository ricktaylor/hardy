use super::*;
use std::sync::Arc;
use tokio::sync::mpsc::*;

pub struct Ingress<M, B>
where
    M: storage::MetadataStorage + Send + Sync,
    B: storage::BundleStorage + Send + Sync,
{
    cache: Arc<cache::Cache<M, B>>,
    tx: Sender<bundle::Bundle>,
}

impl<M, B> Ingress<M, B>
where
    M: storage::MetadataStorage + Send + Sync + 'static,
    B: storage::BundleStorage + Send + Sync + 'static,
{
    pub async fn init(
        _config: &config::Config,
        cache: Arc<cache::Cache<M, B>>,
        task_set: &mut tokio::task::JoinSet<()>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<Arc<Self>, anyhow::Error> {
        // Create a channel for new bundles
        let (tx, mut rx) = channel(16);
        let ingress = Arc::new(Self {
            cache: cache.clone(),
            tx: tx.clone(),
        });

        // Spawn a bundle receiver
        let cancel_token_cloned = cancel_token.clone();
        let ingress_cloned = ingress.clone();
        task_set.spawn(async move {
            loop {
                tokio::select! {
                    bundle = rx.recv() => match bundle {
                        None => break,
                        Some(bundle) => {
                            ingress_cloned.do_something_with_the_bundle(bundle).await;
                        }
                    },
                    _ = cancel_token_cloned.cancelled() => break
                }
            }
        });

        // Perform a cache check
        log::info!("Checking cache...");
        cache.check(cancel_token, tx).await?;
        log::info!("Cache check complete");

        Ok(ingress)
    }

    pub async fn receive(&self, data: Arc<Vec<u8>>) -> Result<bool, anyhow::Error> {
        // Store the bundle in the cache
        let Some(bundle) = self.cache.store(data).await? else {
            return Ok(false);
        };

        // Put bundle into RX queue
        self.tx.send(bundle).await?;

        Ok(true)
    }

    async fn do_something_with_the_bundle(&self, _bundle: bundle::Bundle) {
        // This is the meat of the ingress controller
        todo!()
    }
}
