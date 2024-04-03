use super::*;

pub struct Dispatcher<M, B>
where
    M: storage::MetadataStorage + Send + Sync,
    B: storage::BundleStorage + Send + Sync,
{
    cache: cache::Cache<M, B>,
}

impl<M, B> Clone for Dispatcher<M, B>
where
    M: storage::MetadataStorage + Send + Sync,
    B: storage::BundleStorage + Send + Sync,
{
    fn clone(&self) -> Self {
        Self {
            cache: self.cache.clone(),
        }
    }
}

impl<M, B> Dispatcher<M, B>
where
    M: storage::MetadataStorage + Send + Sync + 'static,
    B: storage::BundleStorage + Send + Sync + 'static,
{
    pub fn new(
        _config: &config::Config,
        cache: cache::Cache<M, B>,
        _task_set: &mut tokio::task::JoinSet<()>,
        _cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<Self, anyhow::Error> {
        // Create a channel for new bundles
        let dispatcher = Self { cache };

        // Spawn a bundle receiver
        /*let cancel_token_cloned = cancel_token.clone();
        let ingress_cloned = ingress.clone();
        task_set.spawn(async move {
            Self::pipeline_pump(ingress_cloned, rx, cancel_token_cloned).await
        });*/

        Ok(dispatcher)
    }

    pub async fn delete_bundle(&self, bundle: bundle::Bundle) -> Result<(), anyhow::Error> {
        // Check if a report is requested
        if bundle.primary.flags.delete_report_requested && !bundle.primary.flags.is_admin_record {}
        todo!()
    }
}
