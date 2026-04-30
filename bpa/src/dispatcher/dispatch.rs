use super::*;
use futures::{FutureExt, join, select_biased};
use hardy_bpv7::status_report::ReasonCode;

impl Dispatcher {
    /// Queue a bundle for dispatch processing
    pub(super) async fn dispatch_bundle(&self, mut bundle: bundle::Bundle) {
        self.store
            .update_status(&mut bundle, &bundle::BundleStatus::Dispatching)
            .await;

        if self.dispatch_tx.send(bundle).await.is_err() {
            debug!("Dispatch queue closed, bundle dropped");
        }
    }

    /// Consumer task for the dispatch queue
    pub(super) async fn run_dispatch_queue(self: Arc<Self>, dispatch_rx: storage::Receiver) {
        while let Ok(Some(bundle)) = dispatch_rx.recv_async().await {
            let dispatcher = self.clone();
            hardy_async::spawn!(self.processing_pool, "process_bundle", async move {
                if let Some(data) = dispatcher.load_data(&bundle).await {
                    dispatcher
                        .process_bundle(bundle, data, dispatcher.cla_registry())
                        .await;
                } else if !bundle.has_expired() {
                    // Bundle data was deleted while queued - not reaped
                    dispatcher
                        .drop_bundle(bundle, ReasonCode::DepletedStorage)
                        .await;
                }
            })
            .await;
        }

        debug!("Dispatch queue consumer exiting");
    }

    /// Routing decision hub: determines bundle disposition based on RIB lookup.
    ///
    /// # Route Results
    ///
    /// | Result | Action | Status Transition |
    /// |--------|--------|-------------------|
    /// | `Drop` | Delete bundle with reason | `Dispatching` → Tombstone |
    /// | `AdminEndpoint` | Handle administrative record | `Dispatching` → Tombstone |
    /// | `Deliver` (fragment) | Queue for reassembly | `Dispatching` → `AduFragment` |
    /// | `Deliver` (whole) | Deliver to service | `Dispatching` → Tombstone |
    /// | `Forward` | Queue to CLA peer | `Dispatching` → `ForwardPending` |
    /// | `None` | Wait for route | `Dispatching` → `Waiting` |
    ///
    /// See [Routing Design](../../docs/routing_subsystem_design.md) for RIB lookup details.
    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    pub(super) async fn process_bundle(
        &self,
        mut bundle: bundle::Bundle,
        data: Bytes,
        cla_registry: &cla::registry::ClaRegistry,
    ) {
        // Perform RIB lookup (sets bundle.metadata.next_hop for Forward results)
        match self.rib.find(&mut bundle) {
            Some(rib::FindResult::Drop(reason)) => {
                if let Some(reason) = reason {
                    debug!("Routing lookup indicates bundle should be dropped: {reason:?}");
                    self.drop_bundle(bundle, reason).await
                } else {
                    debug!("Routing lookup indicates bundle should be dropped without reason");
                    self.delete_bundle(bundle).await
                }
            }
            Some(rib::FindResult::AdminEndpoint) => {
                // The bundle is for the Administrative Endpoint
                self.administrative_bundle(bundle, data).await
            }
            Some(rib::FindResult::Deliver(Some(service))) => {
                // Check for reassembly
                if bundle.bundle.id.fragment_info.is_some() {
                    // Reassemble the bundle before delivery
                    self.reassemble(bundle).await
                } else {
                    // Bundle is for a local service
                    self.deliver_bundle(service, bundle, data).await
                }
            }
            Some(rib::FindResult::Forward(peer)) => {
                debug!("Queuing bundle for forwarding to CLA peer {peer}");
                if let Err(bundle) = cla_registry.forward(peer, bundle).await {
                    debug!("CLA forward failed, returning bundle to watch queue");
                    self.store.watch_bundle(bundle).await;
                }
            }
            Some(rib::FindResult::Deliver(None)) | None => {
                // No route available - wait for one
                debug!("Storing bundle until a forwarding opportunity arises");

                self.store
                    .update_status(&mut bundle, &bundle::BundleStatus::Waiting)
                    .await;
                self.store.watch_bundle(bundle).await
            }
        }
    }

    pub async fn poll_waiting(self: &Arc<Self>, cancel_token: hardy_async::CancellationToken) {
        let (tx, rx) = flume::bounded::<bundle::Bundle>(self.poll_channel_depth);

        let dispatcher = self.clone();

        // Run producer and consumer concurrently
        join!(
            // Producer: feed bundles into channel
            self.store.poll_waiting(tx),
            // Consumer: drain channel into shared processing pool
            async {
                loop {
                    select_biased! {
                        bundle = rx.recv_async().fuse() => {
                            let Ok(bundle) = bundle else {
                                break;
                            };

                            let dispatcher = dispatcher.clone();
                            hardy_async::spawn!(self.processing_pool, "poll_waiting_dispatcher", async move {
                                if let Some(data) = dispatcher.load_data(&bundle).await {
                                    dispatcher.process_bundle(bundle, data, dispatcher.cla_registry()).await
                                } else if !bundle.has_expired() {
                                    // Bundle data was deleted while queued - not reaped
                                    dispatcher.drop_bundle(bundle, ReasonCode::DepletedStorage).await
                                }
                            }).await;
                        }
                        _ = cancel_token.cancelled().fuse() => {
                            break;
                        }
                    }
                }
            }
        );
    }
}
