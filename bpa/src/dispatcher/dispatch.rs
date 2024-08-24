use super::*;

pub(super) enum DispatchResult {
    Done,
    Drop(Option<bpv7::StatusReportReasonCode>),
    Continue,
}

impl Dispatcher {
    #[inline]
    pub async fn dispatch_bundle(&self, bundle: metadata::Bundle) -> Result<(), Error> {
        // Put bundle into channel, ignoring errors as the only ones are intentional
        _ = self.tx.send(bundle).await;
        Ok(())
    }

    #[instrument(skip(self))]
    async fn process_bundle(&self, mut bundle: metadata::Bundle) -> Result<(), Error> {
        loop {
            let result = match &bundle.metadata.status {
                metadata::BundleStatus::IngressPending
                | metadata::BundleStatus::ForwardPending
                | metadata::BundleStatus::Tombstone(_) => {
                    unreachable!()
                }
                metadata::BundleStatus::DispatchPending => {
                    // Check if we are the final destination
                    if self
                        .config
                        .admin_endpoints
                        .is_local_service(&bundle.bundle.destination)
                    {
                        if bundle.bundle.id.fragment_info.is_some() {
                            self.reassemble(&mut bundle).await?
                        } else if self
                            .config
                            .admin_endpoints
                            .is_admin_endpoint(&bundle.bundle.destination)
                        {
                            // The bundle is for the Administrative Endpoint
                            self.administrative_bundle(&mut bundle).await?
                        } else {
                            // The bundle is ready for collection
                            trace!("Bundle is ready for local delivery");
                            self.store
                                .set_status(&mut bundle, metadata::BundleStatus::CollectionPending)
                                .await
                                .map(|_| DispatchResult::Continue)?
                        }
                    } else {
                        // Forward to another BPA
                        self.forward_bundle(&mut bundle).await?
                    }
                }
                metadata::BundleStatus::ReassemblyPending => {
                    // Wait for other fragments to arrive
                    DispatchResult::Done
                }
                metadata::BundleStatus::CollectionPending => {
                    // Check if we have a local service registered
                    if let Some(endpoint) = self
                        .app_registry
                        .find_by_eid(&bundle.bundle.destination)
                        .await
                    {
                        // Notify that the bundle is ready for collection
                        trace!("Notifying application that bundle is ready for collection");
                        endpoint.collection_notify(&bundle.bundle.id).await;
                    }
                    DispatchResult::Done
                }
                metadata::BundleStatus::ForwardAckPending(_, until) => {
                    let until = *until;
                    self.on_bundle_forward_ack(&mut bundle, until).await?
                }
                metadata::BundleStatus::Waiting(until) => {
                    // Check to see if waiting is even worth it
                    let until = *until;
                    self.on_bundle_wait(&mut bundle, until).await?
                }
            };

            match result {
                DispatchResult::Done => return Ok(()),
                DispatchResult::Drop(reason) => return self.drop_bundle(bundle, reason).await,
                DispatchResult::Continue => {}
            }
        }
    }

    pub(super) async fn bundle_wait(
        &self,
        bundle: &mut metadata::Bundle,
        until: time::OffsetDateTime,
    ) -> Result<DispatchResult, Error> {
        // Check to see if waiting is even worth it
        if until > bundle.expiry() {
            trace!("Bundle lifetime is shorter than wait period");
            return Ok(DispatchResult::Drop(Some(
                bpv7::StatusReportReasonCode::NoTimelyContactWithNextNodeOnRoute,
            )));
        }

        let wait = until - time::OffsetDateTime::now_utc();
        if wait > time::Duration::new(self.config.wait_sample_interval as i64, 0) {
            // Nothing to do now, it will be picked up later
            trace!("Bundle will wait offline until: {until}");
            return self
                .store
                .set_status(bundle, metadata::BundleStatus::Waiting(until))
                .await
                .map(|_| DispatchResult::Done);
        }

        trace!("Bundle will wait inline until: {until}");

        // Wait a bit
        if !cancellable_sleep(wait, &self.cancel_token).await {
            // Cancelled
            Ok(DispatchResult::Done)
        } else {
            // Keep dispatching
            Ok(DispatchResult::Continue)
        }
    }

    async fn on_bundle_wait(
        &self,
        bundle: &mut metadata::Bundle,
        until: time::OffsetDateTime,
    ) -> Result<DispatchResult, Error> {
        if until > bundle.expiry() {
            trace!("Bundle lifetime is shorter than wait period");
            return Ok(DispatchResult::Drop(Some(
                bpv7::StatusReportReasonCode::NoTimelyContactWithNextNodeOnRoute,
            )));
        }
        let wait = until - time::OffsetDateTime::now_utc();
        if wait > time::Duration::new(self.config.wait_sample_interval as i64, 0) {
            // Nothing to do now, it will be picked up later
            return Ok(DispatchResult::Done);
        }

        trace!("Bundle will wait inline until: {until}");

        // Wait a bit
        if !cancellable_sleep(wait, &self.cancel_token).await {
            // Cancelled
            Ok(DispatchResult::Done)
        } else {
            // Clear the wait state, and keep dispatching
            self.store
                .set_status(bundle, metadata::BundleStatus::DispatchPending)
                .await
                .map(|_| DispatchResult::Continue)
        }
    }

    async fn on_bundle_forward_ack(
        &self,
        bundle: &mut metadata::Bundle,
        until: time::OffsetDateTime,
    ) -> Result<DispatchResult, Error> {
        // Check to see if waiting is even worth it
        if until > bundle.expiry() {
            trace!("Bundle lifetime is shorter than wait period");
            return Ok(DispatchResult::Drop(Some(
                bpv7::StatusReportReasonCode::NoTimelyContactWithNextNodeOnRoute,
            )));
        }

        // Check if it's worth us waiting inline
        let wait = until - time::OffsetDateTime::now_utc();
        if wait > time::Duration::new(self.config.wait_sample_interval as i64, 0) {
            // Nothing to do now, it will be picked up later
            trace!("Bundle will wait offline until: {until}");
            return Ok(DispatchResult::Done);
        }

        trace!("Bundle will wait inline until: {until}");

        // Wait a bit
        if !cancellable_sleep(wait, &self.cancel_token).await {
            // Cancelled
            return Ok(DispatchResult::Done);
        }

        // Reload bundle after we slept
        match self.store.check_status(&bundle.bundle.id).await? {
            None => {
                // It's gone while we slept
                Ok(DispatchResult::Done)
            }
            Some(status) => {
                if status == bundle.metadata.status {
                    // Clear the wait state
                    self.store
                        .set_status(bundle, metadata::BundleStatus::DispatchPending)
                        .await?;
                } else {
                    bundle.metadata.status = status;
                }
                Ok(DispatchResult::Continue)
            }
        }
    }
}

#[instrument(skip_all)]
pub(super) async fn dispatch_task(
    dispatcher: Arc<Dispatcher>,
    mut rx: tokio::sync::mpsc::Receiver<metadata::Bundle>,
) {
    // We're going to spawn a bunch of tasks
    let mut task_set = tokio::task::JoinSet::new();

    // Give some feedback
    const SECS: u64 = 5;
    let timer = tokio::time::sleep(tokio::time::Duration::from_secs(SECS));
    tokio::pin!(timer);
    let mut bundles_processed = 0u64;

    loop {
        tokio::select! {
            () = &mut timer => {
                if bundles_processed != 0 {
                    info!("{bundles_processed} bundles processed, {} bundles/s",bundles_processed / SECS);
                    bundles_processed = 0;
                }
                timer.as_mut().reset(tokio::time::Instant::now() + tokio::time::Duration::from_secs(SECS));
            },
            bundle = rx.recv() => {
                let dispatcher = dispatcher.clone();
                let bundle = bundle.trace_expect("Dispatcher channel unexpectedly closed");

                task_set.spawn(async move {
                    dispatcher.process_bundle(bundle).await.trace_expect("Failed to dispatch bundle");
                });
            },
            Some(r) = task_set.join_next(), if !task_set.is_empty() => {
                r.trace_expect("Task terminated unexpectedly");

                bundles_processed = bundles_processed.saturating_add(1);
            },
            _ = dispatcher.cancel_token.cancelled() => break
        }
    }

    // Wait for all sub-tasks to complete
    while let Some(r) = task_set.join_next().await {
        r.trace_expect("Task terminated unexpectedly")
    }
}
