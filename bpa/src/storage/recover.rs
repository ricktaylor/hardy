use super::*;
use futures::{FutureExt, join, select_biased};

impl Store {
    pub fn recover(self: &Arc<Self>, dispatcher: &Arc<dispatcher::Dispatcher>) {
        let store = self.clone();
        let dispatcher = dispatcher.clone();

        hardy_async::spawn!(self.tasks, "store_check_task", async move {
            info!("Starting store consistency check...");

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

            if !store.tasks.is_cancelled() {
                store.metadata_storage_recovery(dispatcher).await;
            }

            if !store.tasks.is_cancelled() {
                info!("Store consistency check completed");
            }
        });
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn start_metadata_storage_recovery(&self) {
        self.metadata_storage.start_recovery().await;
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn bundle_storage_recovery(self: &Arc<Self>, dispatcher: Arc<dispatcher::Dispatcher>) {
        let cancel_token = self.tasks.cancel_token().clone();
        let (tx, rx) = flume::bounded::<storage::RecoveryResponse>(16);

        join!(
            async {
                self.bundle_storage
                    .recover(tx)
                    .await
                    .trace_expect("Bundle storage recover failed");
            },
            async {
                loop {
                    select_biased! {
                        r = rx.recv_async().fuse() => {
                            let Some((storage_name, file_time)) = r.ok() else {
                                break;
                            };
                            if let Err(e) = dispatcher.restart_bundle(storage_name.clone(), file_time).await {
                                e.increment_metric();
                                error!("Failed to restart bundle {storage_name}: {e}");
                            }
                        }
                        _ = cancel_token.cancelled().fuse() => {
                            break;
                        }
                    }
                }
            }
        );
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn metadata_storage_recovery(self: &Arc<Self>, dispatcher: Arc<dispatcher::Dispatcher>) {
        let cancel_token = self.tasks.cancel_token().clone();
        let (tx, rx) = flume::bounded::<bundle::Bundle>(16);

        join!(
            async {
                self.metadata_storage
                    .remove_unconfirmed(tx)
                    .await
                    .trace_expect("Remove unconfirmed bundles failed");
            },
            async {
                loop {
                    select_biased! {
                        bundle = rx.recv_async().fuse() => {
                            let Some(bundle) = bundle.ok() else {
                                break;
                            };
                            metrics::counter!("restart_orphan_bundles").increment(1);

                            dispatcher.report_bundle_deletion(
                                &bundle,
                                hardy_bpv7::status_report::ReasonCode::DepletedStorage,
                            )
                            .await
                        },
                        _ = cancel_token.cancelled().fuse() => {
                            break;
                        }
                    }
                }
            }
        );
    }
}
