use super::*;
use futures::{FutureExt, join, select_biased};

impl Store {
    pub fn recover(self: &Arc<Self>, dispatcher: &Arc<dispatcher::Dispatcher>) {
        // Start the store - this can take a while as the store is walked
        let store = self.clone();
        let dispatcher = dispatcher.clone();
        hardy_async::spawn!(self.tasks, "store_check_task", async move {
            // Start the store - this can take a while as the store is walked
            info!("Starting store consistency check...");

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
            // Producer: recover bundles from storage
            async {
                self.bundle_storage
                    .recover(tx)
                    .await
                    .trace_expect("Bundle storage recover failed");
            },
            // Consumer: process recovered bundles
            async {
                loop {
                    select_biased! {
                        r = rx.recv_async().fuse() => match r {
                            Err(_) => {
                                break;
                            }
                            Ok(r) => {
                                dispatcher.restart_bundle(r.0, r.1).await;
                            }
                        },
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
            // Producer: find unconfirmed bundles
            async {
                self.metadata_storage
                    .remove_unconfirmed(tx)
                    .await
                    .trace_expect("Remove unconfirmed bundles failed");
            },
            // Consumer: report orphaned bundles
            async {
                loop {
                    select_biased! {
                        bundle = rx.recv_async().fuse() => match bundle {
                            Err(_) => break,
                            Ok(bundle) => {
                                metrics::counter!("bpa.restart.orphan").increment(1);

                                // The data associated with `bundle` has gone!
                                dispatcher.report_bundle_deletion(
                                    &bundle,
                                    hardy_bpv7::status_report::ReasonCode::DepletedStorage,
                                )
                                .await
                            }
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
