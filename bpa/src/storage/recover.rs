use super::*;

pub enum RestartResult {
    Missing,
    Duplicate,
    Valid,
    Orphan,
    Junk,
}

impl Store {
    pub fn recover(self: &Arc<Self>, dispatcher: &Arc<dispatcher::Dispatcher>) {
        // Start the store - this can take a while as the store is walked
        let store = self.clone();
        let dispatcher = dispatcher.clone();
        let task = async move {
            // Start the store - this can take a while as the store is walked
            info!("Starting store consistency check...");

            // Set up the metrics
            metrics::describe_counter!(
                "restart_lost_bundles",
                metrics::Unit::Count,
                "Total number of lost bundles discovered during storage restart"
            );
            metrics::describe_counter!(
                "restart_duplicate_bundles",
                metrics::Unit::Count,
                "Total number of duplicate bundles discovered during storage restart"
            );
            metrics::describe_counter!(
                "restart_valid_bundles",
                metrics::Unit::Count,
                "Total number of valid bundles discovered during storage restart"
            );
            metrics::describe_counter!(
                "restart_orphan_bundles",
                metrics::Unit::Count,
                "Total number of orphaned bundles discovered during storage restart"
            );
            metrics::describe_counter!(
                "restart_junk_bundles",
                metrics::Unit::Count,
                "Total number of junk bundles discovered during storage restart"
            );

            store.start_metadata_storage_recovery().await;

            store.bundle_storage_recovery(dispatcher.clone()).await;

            if !store.cancel_token.is_cancelled() {
                store.metadata_storage_recovery(dispatcher).await;
            }
        };

        #[cfg(feature = "tracing")]
        let task = {
            let span = tracing::trace_span!(parent: None, "store_check_task");
            span.follows_from(tracing::Span::current());
            task.instrument(span)
        };

        self.task_tracker.spawn(task);
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    async fn start_metadata_storage_recovery(&self) {
        self.metadata_storage.start_recovery().await;
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    async fn bundle_storage_recovery(self: &Arc<Self>, dispatcher: Arc<dispatcher::Dispatcher>) {
        let cancel_token = self.cancel_token.clone();
        let (tx, rx) = flume::bounded::<storage::RecoveryResponse>(16);
        let task = async move {
            loop {
                tokio::select! {
                    r = rx.recv_async() => match r {
                        Err(_) => {
                            break;
                        }
                        Ok(r) => {
                            match dispatcher.restart_bundle(r.0,r.1).await {
                                RestartResult::Missing => metrics::counter!("restart_lost_bundles").increment(1),
                                RestartResult::Duplicate => metrics::counter!("restart_duplicate_bundles").increment(1),
                                RestartResult::Valid => metrics::counter!("restart_valid_bundles").increment(1),
                                RestartResult::Orphan => metrics::counter!("restart_orphan_bundles").increment(1),
                                RestartResult::Junk => metrics::counter!("restart_junk_bundles").increment(1),
                            }
                        }
                    },
                    _ = cancel_token.cancelled() => {
                        break;
                    }
                }
            }
        };

        #[cfg(feature = "tracing")]
        let task = {
            let span = tracing::trace_span!(parent: None, "bundle_storage_recovery_reader");
            span.follows_from(tracing::Span::current());
            task.instrument(span)
        };

        let h = tokio::spawn(task);

        self.bundle_storage
            .recover(tx)
            .await
            .trace_expect("Bundle storage recover failed");

        _ = h.await;
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    async fn metadata_storage_recovery(self: &Arc<Self>, dispatcher: Arc<dispatcher::Dispatcher>) {
        let cancel_token = self.cancel_token.clone();
        let (tx, rx) = flume::bounded::<bundle::Bundle>(16);
        let task = async move {
            loop {
                tokio::select! {
                    bundle = rx.recv_async() => match bundle {
                        Err(_) => break,
                        Ok(bundle) => {
                            metrics::counter!("restart_orphan_bundles").increment(1);

                            // The data associated with `bundle` has gone!
                            dispatcher.report_bundle_deletion(
                                &bundle,
                                hardy_bpv7::status_report::ReasonCode::DepletedStorage,
                            )
                            .await
                        }
                    },
                    _ = cancel_token.cancelled() => {
                        break;
                    }
                }
            }
        };

        #[cfg(feature = "tracing")]
        let task = {
            let span = tracing::trace_span!(parent: None, "metadata_storage_check_reader");
            span.follows_from(tracing::Span::current());
            task.instrument(span)
        };

        let h = tokio::spawn(task);

        self.metadata_storage
            .remove_unconfirmed(tx)
            .await
            .trace_expect("Remove unconfirmed bundles failed");

        _ = h.await;
    }
}
