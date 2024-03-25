use super::*;
use std::sync::Arc;
use tokio::sync::mpsc::*;

pub struct Ingress<M, B>
where
    M: storage::MetadataStorage + Send + Sync,
    B: storage::BundleStorage + Send + Sync,
{
    cache: Arc<cache::Cache<M, B>>,
    tx: Sender<(bundle::Bundle, bool)>,
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
        let (tx, rx) = channel(16);
        let ingress = Arc::new(Self {
            cache: cache.clone(),
            tx: tx.clone(),
        });

        // Spawn a bundle receiver
        let cancel_token_cloned = cancel_token.clone();
        let ingress_cloned = ingress.clone();
        task_set.spawn(async move {
            Self::pipeline_pump(ingress_cloned, rx, cancel_token_cloned).await
        });

        // Perform a cache check
        log::info!("Starting cache reload...");

        tokio::task::spawn_blocking(move || {
            cache.check(cancel_token, tx)
        }).await??;
        log::info!("Cache reload complete");

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

    async fn pipeline_pump(
        ingress: Arc<Self>,
        mut rx: Receiver<(bundle::Bundle, bool)>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        // We're going to spawn a bunch of tasks
        let mut task_set = tokio::task::JoinSet::new();

        loop {
            tokio::select! {
                bundle = rx.recv() => match bundle {
                    None => break,
                    Some((bundle,valid)) => {
                        let ingress = ingress.clone();
                        task_set.spawn(async move {
                            ingress.do_something_with_the_bundle(bundle,valid).await;
                        });
                    }
                },
                _ = cancel_token.cancelled() => break
            }
        }

        // Wait for all sub-tasks to complete
        while let Some(r) = task_set.join_next().await {
            r.log_expect("Task terminated unexpectedly")
        }
    }

    async fn do_something_with_the_bundle(&self, _bundle: bundle::Bundle, valid: bool) {
        // This is the meat of the ingress pipeline
        todo!()
    }
}
