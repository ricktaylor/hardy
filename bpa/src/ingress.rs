use super::*;
use tokio::sync::mpsc::*;

pub type ClaSource = Option<(String, Vec<u8>)>;

pub struct Ingress {
    cache: cache::Cache,
    dispatcher: dispatcher::Dispatcher,
    tx: Sender<(ClaSource, bundle::Metadata, bundle::Bundle, bool)>,
}

impl Clone for Ingress {
    fn clone(&self) -> Self {
        Self {
            cache: self.cache.clone(),
            dispatcher: self.dispatcher.clone(),
            tx: self.tx.clone(),
        }
    }
}

impl Ingress {
    pub fn new(
        _config: &config::Config,
        cache: cache::Cache,
        dispatcher: dispatcher::Dispatcher,
        task_set: &mut tokio::task::JoinSet<()>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<Self, anyhow::Error> {
        // Create a channel for new bundles
        let (tx, rx) = channel(16);
        let ingress = Self {
            cache,
            dispatcher,
            tx,
        };

        // Spawn a bundle receiver
        let ingress_cloned = ingress.clone();
        task_set.spawn(async move { Self::pipeline_pump(ingress_cloned, rx, cancel_token).await });

        Ok(ingress)
    }

    pub async fn receive(&self, from: ClaSource, data: Vec<u8>) -> Result<bool, anyhow::Error> {
        // Parse the bundle
        let (bundle, valid) = match bundle::Bundle::parse(&data) {
            Ok(r) => r,
            Err(e) => {
                // Parse failed badly, no idea who to report to
                log::info!("Bundle parsing failed: {}", e);
                return Ok(false);
            }
        };

        // Store the bundle in the cache
        let metadata = self
            .cache
            .store(&bundle, data, bundle::BundleStatus::DispatchPending)
            .await?;

        // Report we have received the bundle
        self.dispatcher
            .report_bundle_reception(&metadata, &bundle)
            .await?;

        // Enqueue bundle
        self.enqueue_bundle(from, metadata, bundle, valid).await?;

        // We have processed it, caller doesn't have to react
        Ok(true)
    }

    pub async fn enqueue_bundle(
        &self,
        from: ClaSource,
        metadata: bundle::Metadata,
        bundle: bundle::Bundle,
        valid: bool,
    ) -> Result<(), anyhow::Error> {
        // Put bundle into channel
        self.tx
            .send((from, metadata, bundle, valid))
            .await
            .map_err(|e| e.into())
    }

    async fn pipeline_pump(
        self,
        mut rx: Receiver<(ClaSource, bundle::Metadata, bundle::Bundle, bool)>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        // We're going to spawn a bunch of tasks
        let mut task_set = tokio::task::JoinSet::new();

        loop {
            tokio::select! {
                bundle = rx.recv() => match bundle {
                    None => break,
                    Some((cla_source,metadata,bundle,valid)) => {
                        let ingress = self.clone();
                        task_set.spawn(async move {
                            ingress.do_something_with_the_bundle(cla_source,metadata,bundle,valid).await;
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
        metadata: bundle::Metadata,
        bundle: bundle::Bundle,
        valid: bool,
    ) -> Result<(), anyhow::Error> {
        // This is the meat of the ingress pipeline
        todo!()
    }
}
