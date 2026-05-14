use futures::{FutureExt, join, select_biased};
use hardy_bpv7::status_report::ReasonCode;
use trace_err::*;
use tracing::info;
#[cfg(feature = "instrument")]
use tracing::instrument;

use super::{RecoveryResponse, Store};
use crate::Arc;
use crate::bundle::Bundle;
use crate::dispatcher::Dispatcher;
use crate::recover;

impl Store {
    pub fn recover(self: &Arc<Self>, dispatcher: &Arc<Dispatcher>) {
        let store = self.clone();
        let dispatcher = dispatcher.clone();
        hardy_async::spawn!(self.tasks, "store_check_task", async move {
            info!("Starting store consistency check...");

            store.start_metadata_storage_recovery().await;

            store.bundle_storage_recovery(&dispatcher).await;

            if !store.tasks.is_cancelled() {
                store.metadata_storage_recovery(&dispatcher).await;
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
    async fn bundle_storage_recovery(self: &Arc<Self>, dispatcher: &Dispatcher) {
        let cancel_token = self.tasks.cancel_token().clone();
        let (tx, rx) = flume::bounded::<RecoveryResponse>(16);

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
                        r = rx.recv_async().fuse() => match r {
                            Err(_) => break,
                            Ok(r) => {
                                recover::recover_bundle(
                                    r.0,
                                    r.1,
                                    self,
                                    &dispatcher.key_store,
                                    &dispatcher.dispatch_tx,
                                )
                                .await;
                            }
                        },
                        _ = cancel_token.cancelled().fuse() => break,
                    }
                }
            }
        );
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn metadata_storage_recovery(self: &Arc<Self>, dispatcher: &Dispatcher) {
        let cancel_token = self.tasks.cancel_token().clone();
        let (tx, rx) = flume::bounded::<Bundle>(16);

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
                        bundle = rx.recv_async().fuse() => match bundle {
                            Err(_) => break,
                            Ok(bundle) => {
                                metrics::counter!("bpa.restart.orphan").increment(1);
                                // TODO: extract to reporting.rs
                                dispatcher.report_bundle_deletion(
                                    &bundle,
                                    ReasonCode::DepletedStorage,
                                )
                                .await
                            }
                        },
                        _ = cancel_token.cancelled().fuse() => break,
                    }
                }
            }
        );
    }
}
