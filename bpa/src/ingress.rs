use super::*;
use std::sync::Arc;
use tokio::sync::mpsc::*;

pub type ClaSource = Option<(String, Vec<u8>)>;

pub struct Ingress<M, B>
where
    M: storage::MetadataStorage + Send + Sync,
    B: storage::BundleStorage + Send + Sync,
{
    cache: cache::Cache<M, B>,
    tx: Sender<(ClaSource, bundle::Bundle, bool)>,
}

impl<M, B> Clone for Ingress<M, B>
where
    M: storage::MetadataStorage + Send + Sync,
    B: storage::BundleStorage + Send + Sync,
{
    fn clone(&self) -> Self {
        Self {
            cache: self.cache.clone(),
            tx: self.tx.clone(),
        }
    }
}

impl<M, B> Ingress<M, B>
where
    M: storage::MetadataStorage + Send + Sync + 'static,
    B: storage::BundleStorage + Send + Sync + 'static,
{
    pub fn new(
        _config: &config::Config,
        cache: cache::Cache<M, B>,
        task_set: &mut tokio::task::JoinSet<()>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<Self, anyhow::Error> {
        // Create a channel for new bundles
        let (tx, rx) = channel(16);
        let ingress = Self { cache, tx };

        // Spawn a bundle receiver
        let cancel_token_cloned = cancel_token.clone();
        let ingress_cloned = ingress.clone();
        task_set.spawn(async move {
            Self::pipeline_pump(ingress_cloned, rx, cancel_token_cloned).await
        });

        Ok(ingress)
    }

    pub async fn receive(
        &self,
        cla_source: ClaSource,
        data: Vec<u8>,
    ) -> Result<bool, anyhow::Error> {
        // Store the bundle in the cache
        let (Some(bundle), valid) = self.cache.store(Arc::new(data)).await? else {
            return Ok(false);
        };

        // Put bundle into RX queue
        self.tx.send((cla_source, bundle, valid)).await?;

        // We have processed it, caller doesn't have to react
        Ok(true)
    }

    async fn pipeline_pump(
        self,
        mut rx: Receiver<(ClaSource, bundle::Bundle, bool)>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        // We're going to spawn a bunch of tasks
        let mut task_set = tokio::task::JoinSet::new();

        loop {
            tokio::select! {
                bundle = rx.recv() => match bundle {
                    None => break,
                    Some((cla_source,bundle,valid)) => {
                        let ingress = self.clone();
                        task_set.spawn(async move {
                            ingress.do_something_with_the_bundle(cla_source,bundle,valid).await;
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

    async fn do_something_with_the_bundle(
        &self,
        cla_source: ClaSource,
        bundle: bundle::Bundle,
        valid: bool,
    ) {
        // This is the meat of the ingress pipeline
        todo!()
    }
}
