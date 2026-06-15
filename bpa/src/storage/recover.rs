use futures::{FutureExt, join, select_biased};
use hardy_bpv7::status_report::ReasonCode;
use trace_err::*;
use tracing::info;

#[cfg(feature = "instrument")]
use tracing::instrument;

use crate::{Arc, bundle::Bundle, dispatcher::Dispatcher, stream::ChannelSender};

use super::{RecoveryResponse, store::Store};

impl Store {
    pub fn recover(self: &Arc<Self>, dispatcher: &Arc<Dispatcher>) {
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
    async fn bundle_storage_recovery(self: &Arc<Self>, dispatcher: Arc<Dispatcher>) {
        let cancel_token = self.tasks.cancel_token().clone();
        let (stream, rx) = ChannelSender::<RecoveryResponse>::bounded(16);

        join!(
            // Producer: recover bundles from storage
            async {
                // Race against cancel so the producer can't block on a full
                // channel after the consumer breaks (join! keeps rx alive).
                select_biased! {
                    r = self.bundle_storage.recover(&stream).fuse() => {
                        r.trace_expect("Bundle storage recover failed");
                    }
                    _ = cancel_token.cancelled().fuse() => {}
                }
                drop(stream);
            },
            // Consumer: process recovered bundles
            async {
                loop {
                    select_biased! {
                        r = rx.recv().fuse() => match r {
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
    async fn metadata_storage_recovery(self: &Arc<Self>, dispatcher: Arc<Dispatcher>) {
        let cancel_token = self.tasks.cancel_token().clone();
        let (stream, rx) = ChannelSender::<Bundle>::bounded(16);

        join!(
            // Producer: find unconfirmed bundles
            async {
                // Race against cancel so the producer can't block on a full
                // channel after the consumer breaks (join! keeps rx alive).
                select_biased! {
                    r = self.metadata_storage.remove_unconfirmed(&stream).fuse() => {
                        r.trace_expect("Remove unconfirmed bundles failed");
                    }
                    _ = cancel_token.cancelled().fuse() => {}
                }
                drop(stream);
            },
            // Consumer: report orphaned bundles
            async {
                loop {
                    select_biased! {
                        bundle = rx.recv().fuse() => match bundle {
                            Err(_) => break,
                            Ok(bundle) => {
                                metrics::counter!("bpa.restart.orphan").increment(1);

                                // The data associated with `bundle` has gone!
                                dispatcher.report_bundle_deletion(
                                    &bundle,
                                    ReasonCode::DepletedStorage,
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
