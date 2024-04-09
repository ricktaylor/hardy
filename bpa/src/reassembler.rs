use super::*;
use tokio::sync::mpsc::*;

pub struct Reassembler {
    store: store::Store,
    dispatcher: dispatcher::Dispatcher,
    tx: Sender<(bundle::Metadata, bundle::Bundle)>,
}

impl Clone for Reassembler {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            dispatcher: self.dispatcher.clone(),
            tx: self.tx.clone(),
        }
    }
}

impl Reassembler {
    pub fn new(
        _config: &config::Config,
        store: store::Store,
        dispatcher: dispatcher::Dispatcher,
        task_set: &mut tokio::task::JoinSet<()>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<Self, anyhow::Error> {
        // Create a channel for new bundles
        let (tx, rx) = channel(16);
        let reassembler = Self {
            store,
            dispatcher,
            tx,
        };

        // Spawn a bundle receiver
        let reassembler_cloned = reassembler.clone();
        task_set
            .spawn(async move { Self::pipeline_pump(reassembler_cloned, rx, cancel_token).await });

        Ok(reassembler)
    }

    pub async fn enqueue_bundle(
        &self,
        metadata: bundle::Metadata,
        bundle: bundle::Bundle,
    ) -> Result<(), anyhow::Error> {
        // Put bundle into channel
        self.tx.send((metadata, bundle)).await.map_err(|e| e.into())
    }

    async fn pipeline_pump(
        self,
        mut rx: Receiver<(bundle::Metadata, bundle::Bundle)>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        // We're going to spawn a bunch of tasks
        let mut task_set = tokio::task::JoinSet::new();

        loop {
            tokio::select! {
                bundle = rx.recv() => match bundle {
                    None => break,
                    Some((metadata,bundle)) => {
                        let reassembler = self.clone();
                        task_set.spawn(async move {
                            reassembler.process_bundle(metadata,bundle).await.log_expect("Failed to process bundle")
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

    async fn process_bundle(
        &self,
        metadata: bundle::Metadata,
        bundle: bundle::Bundle,
    ) -> Result<(), anyhow::Error> {
        let bundle::BundleStatus::ReassemblyPending = metadata.status else {
            panic!(
                "Reassembler processed bundle with state {:?}",
                &metadata.status
            )
        };

        todo!()
    }
}
