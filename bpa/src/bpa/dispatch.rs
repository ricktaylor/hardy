use alloc::sync::Arc;

use futures::{FutureExt, join, select_biased};
use tracing::debug;

use super::Bpa;
use crate::bundle;
use crate::storage;

impl Bpa {
    /// Queue a bundle for dispatch processing.
    /// The caller must ensure the bundle status is already `Dispatching`.
    pub(super) async fn dispatch_bundle(&self, bundle: bundle::Bundle) {
        debug_assert!(matches!(
            bundle.metadata.status,
            bundle::BundleStatus::Dispatching
        ));

        if self.dispatch_tx.send(bundle).await.is_err() {
            debug!("Dispatch queue closed, bundle dropped");
        }
    }

    /// Consumer task for the dispatch queue.
    /// Performs a fresh RIB lookup and dispatches the result.
    pub(super) async fn run_dispatch_queue(self: Arc<Self>, dispatch_rx: storage::Receiver) {
        while let Ok(Some(bundle)) = dispatch_rx.recv_async().await {
            let bpa = self.clone();
            hardy_async::spawn!(self.processing_pool, "process_bundle", async move {
                bpa.rib_lookup_and_dispatch(bundle).await;
            })
            .await;
        }

        debug!("Dispatch queue consumer exiting");
    }

    /// Fresh RIB lookup followed by dispatch. Used by the dispatch queue
    /// and poll_waiting for bundles that need a (re-)routing decision.
    async fn rib_lookup_and_dispatch(&self, mut bundle: bundle::Bundle) {
        let route = self.rib.find(&mut bundle);
        self.dispatch(bundle, route).await;
    }

    pub async fn poll_waiting(self: &Arc<Self>, cancel_token: hardy_async::CancellationToken) {
        let (tx, rx) = flume::bounded::<bundle::Bundle>(self.poll_channel_depth);

        let bpa = self.clone();

        join!(self.store.poll_waiting(tx), async {
            loop {
                select_biased! {
                    bundle = rx.recv_async().fuse() => {
                        let Ok(bundle) = bundle else {
                            break;
                        };

                        let bpa = bpa.clone();
                        hardy_async::spawn!(self.processing_pool, "poll_waiting_dispatcher", async move {
                            bpa.rib_lookup_and_dispatch(bundle).await
                        }).await;
                    }
                    _ = cancel_token.cancelled().fuse() => {
                        break;
                    }
                }
            }
        });
    }
}
