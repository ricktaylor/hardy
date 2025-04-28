use super::*;

pub(super) enum DispatchResult {
    Done,
    Drop(Option<bpv7::StatusReportReasonCode>),
    Continue,
}

impl Dispatcher {
    #[inline]
    pub async fn dispatch_bundle(&self, bundle: bundle::Bundle) {
        // Put bundle into channel, ignoring errors as the only ones are intentional
        _ = self.tx.send(bundle).await;
    }

    #[instrument(skip(self))]
    pub async fn process_bundle(&self, mut bundle: bundle::Bundle) -> Result<(), Error> {
        /* This is a classic looped state machine */
        loop {
            let result = match &bundle.metadata.status {
                BundleStatus::Tombstone(_) => {
                    unreachable!()
                }
                BundleStatus::DispatchPending => {
                    // Check if we are the final destination
                    if self
                        .admin_endpoints
                        .is_local_service(&bundle.bundle.destination)
                    {
                        if bundle.bundle.id.fragment_info.is_some() {
                            self.reassemble(&mut bundle).await?
                        } else if self.admin_endpoints.contains(&bundle.bundle.destination) {
                            // The bundle is for the Administrative Endpoint
                            self.administrative_bundle(&mut bundle).await?
                        } else {
                            // The bundle is ready for collection
                            trace!("Bundle is ready for local delivery");
                            self.store
                                .set_status(&mut bundle, BundleStatus::CollectionPending)
                                .await
                                .map(|_| DispatchResult::Continue)?
                        }
                    } else {
                        // Forward to another BPA
                        self.forward_bundle(&mut bundle).await?
                    }
                }
                BundleStatus::ReassemblyPending => {
                    // Wait for other fragments to arrive
                    DispatchResult::Done
                }
                BundleStatus::CollectionPending => {
                    // Check if we have a local service registered
                    if let Some(service) =
                        self.service_registry.find(&bundle.bundle.destination).await
                    {
                        // Notify that the bundle is ready for collection
                        trace!("Notifying application that bundle is ready for collection");
                        service
                            .on_received(&bundle.bundle.id, bundle.expiry())
                            .await;
                    }
                    DispatchResult::Done
                }
            };

            match result {
                DispatchResult::Done => return Ok(()),
                DispatchResult::Drop(reason) => return self.drop_bundle(bundle, reason).await,
                DispatchResult::Continue => {}
            }
        }
    }

    #[instrument(skip_all)]
    pub async fn run(self: Arc<Dispatcher>, mut rx: tokio::sync::mpsc::Receiver<bundle::Bundle>) {
        // We're going to spawn a bunch of tasks
        let mut task_set = tokio::task::JoinSet::new();

        // Start the store - this can take a while as the store is walked
        self.store
            .start(self.clone(), self.cancel_token.clone())
            .await;

        // Give some feedback
        const SECS: u64 = 5;
        let mut timer = tokio::time::interval(tokio::time::Duration::from_secs(SECS));
        let mut bundles_processed = 0u64;

        while !task_set.is_empty() || !rx.is_closed() {
            tokio::select! {
                _ = timer.tick() => {
                    if bundles_processed != 0 {
                        info!("{bundles_processed} bundles processed, {} bundles/s",bundles_processed / SECS);
                        bundles_processed = 0;
                    }
                },
                Some(bundle) = rx.recv(), if !rx.is_closed() =>  {
                    let dispatcher = self.clone();
                    task_set.spawn(async move {
                        dispatcher.process_bundle(bundle).await.trace_expect("Failed to dispatch bundle");
                    });
                },
                Some(r) = task_set.join_next(), if !task_set.is_empty() => {
                    r.trace_expect("Task terminated unexpectedly");
                    bundles_processed = bundles_processed.saturating_add(1);
                },
                _ = self.cancel_token.cancelled(), if !rx.is_closed() => {
                    // Close the queue, we're done
                    rx.close();
                }
            }
        }
    }
}
