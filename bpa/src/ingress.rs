use super::*;
use tokio::sync::mpsc::*;

pub type ClaSource = Option<(String, Vec<u8>)>;

pub struct Ingress {
    store: store::Store,
    dispatcher: dispatcher::Dispatcher,
    tx: Sender<(ClaSource, bundle::Metadata, bundle::Bundle, bool)>,
}

impl Clone for Ingress {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            dispatcher: self.dispatcher.clone(),
            tx: self.tx.clone(),
        }
    }
}

impl Ingress {
    pub fn new(
        _config: &config::Config,
        store: store::Store,
        dispatcher: dispatcher::Dispatcher,
        task_set: &mut tokio::task::JoinSet<()>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<Self, anyhow::Error> {
        // Create a channel for new bundles
        let (tx, rx) = channel(16);
        let ingress = Self {
            store,
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

        // Store the bundle in the store
        let metadata = self
            .store
            .store(&bundle, data, bundle::BundleStatus::IngressPending)
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
                            ingress.process_bundle(cla_source,metadata,bundle,valid).await;
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
        cla_source: ClaSource,
        mut metadata: bundle::Metadata,
        bundle: bundle::Bundle,
        valid: bool,
    ) -> Result<(), anyhow::Error> {
        // This is the meat of the ingress pipeline
        loop {
            // Duff's device
            match &metadata.status {
                bundle::BundleStatus::IngressPending => {
                    // Report we have received the bundle
                    self.dispatcher
                        .report_bundle_reception(&metadata, &bundle)
                        .await?;

                    // Valid is only negative if the bundle came from an orphan check, so we must check here
                    if !valid {
                        // Unintelligible bundle
                        self.dispatcher
                            .report_bundle_deletion(
                                &metadata,
                                &bundle,
                                dispatcher::BundleStatusReportReasonCode::BlockUnintelligible,
                            )
                            .await?;

                        // Drop the bundle
                        return self.store.remove(&bundle.id).await;
                    }

                    /* RACE: If there is a crash between the report creation(above) and the status update (below)
                     * then we may send more than one "Received" Status Report, but that is currently considered benign and unlikely ;)
                     */

                    metadata.status = self
                        .store
                        .set_bundle_status(&bundle.id, bundle::BundleStatus::IngressFilterPending)
                        .await?;
                }
                bundle::BundleStatus::IngressFilterPending => todo!(),
                //bundle::BundleStatus::ForwardPending => todo!(),
                //bundle::BundleStatus::ReassemblyPending => todo!(),
                _ => break,
            }
        }

        todo!()
    }
}
