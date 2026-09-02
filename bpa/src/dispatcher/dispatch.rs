use futures::{FutureExt, join, select_biased};

use super::*;

impl Dispatcher {
    /// Queue a bundle for dispatch processing.
    /// The caller must have claimed the bundle (`Dispatching`), or be
    /// re-queueing one recovered still queued (`DispatchPending`); the send's
    /// conditional swap moves it to `DispatchPending`, the queue's commit
    /// point, until the consumer claims it back.
    pub(super) async fn dispatch_bundle(&self, bundle: bundle::Bundle) {
        debug_assert!(matches!(
            bundle.status,
            bundle::BundleStatus::Dispatching | bundle::BundleStatus::DispatchPending
        ));

        if self.dispatch_tx.send(bundle).await.is_err() {
            debug!("Dispatch queue closed, bundle dropped");
        }
    }

    /// Consumer task for the dispatch queue
    pub(super) async fn run_dispatch_queue(
        self: Arc<Self>,
        dispatch_rx: hardy_async::closeable::Receiver<bundle::Bundle>,
    ) {
        while let Ok(mut bundle) = dispatch_rx.recv().await {
            // Claim the bundle out of DispatchPending before spending a pool
            // slot on it: the channel is at-least-once (its storage poller
            // can push a copy already snapshotted before this claim), so a
            // copy that loses the swap is a duplicate and is dropped here.
            if !self
                .store
                .swap_status(&mut bundle, &bundle::BundleStatus::Dispatching)
                .await
            {
                debug!("Bundle already claimed for processing, dropping duplicate copy");
                continue;
            }

            let dispatcher = self.clone();
            hardy_async::spawn!(self.processing_pool, "process_bundle", async move {
                dispatcher
                    .process_bundle(bundle, dispatcher.cla_registry())
                    .await;
            })
            .await;
        }

        debug!("Dispatch queue consumer exiting");
    }

    /// Routing decision hub: determines bundle disposition based on RIB lookup.
    ///
    /// Bundle data is loaded lazily — only the `AdminEndpoint` and `Deliver`
    /// paths need it immediately. `Forward` defers loading to `forward_bundle`
    /// (after dequeue from the peer's backpressure channel).
    ///
    /// # Route Results
    ///
    /// | Result | Action | Status Transition |
    /// |--------|--------|-------------------|
    /// | `Drop` | Delete bundle with reason | `Dispatching` → Tombstone |
    /// | `AdminEndpoint` | Handle administrative record | `Dispatching` → Tombstone |
    /// | `Deliver` (fragment) | Queue for reassembly | `Dispatching` → `AduFragment` |
    /// | `Deliver` (whole) | Queue to the service | `Dispatching` → `DeliverPending` |
    /// | `Deliver` (service gone) | Wait for a service | `Dispatching` → `WaitingForService` |
    /// | `Forward` | Queue to CLA peer | `Dispatching` → `ForwardPending` |
    /// | `Forward` (peer gone) | Wait for route | `Dispatching` → `Waiting` |
    /// | `None` (local destination) | Wait for a service | `Dispatching` → `WaitingForService` |
    /// | `None` | Wait for route | `Dispatching` → `Waiting` |
    ///
    /// See [Routing Design](../../docs/routing_subsystem_design.md) for RIB lookup details.
    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.id())))]
    pub(super) async fn process_bundle(
        &self,
        bundle: bundle::Bundle,
        cla_registry: &cla::registry::ClaRegistry,
    ) {
        // Expiry checkpoint: the reaper defers the hand-off statuses
        // (DeliveryAckPending/ForwardAckPending), so an expired bundle can
        // re-enter dispatch through a transfer outcome, a sweep, or a poll —
        // resolve it here rather than routing it onward.
        if bundle.has_expired() {
            return self.drop_bundle(bundle, ReasonCode::LifetimeExpired).await;
        }

        // Snapshot the routing table before the lookup: the parks below
        // re-check it to close the park-vs-poll window (see park_bundle).
        let seen = self.rib.table_snapshot();

        // Perform RIB lookup; a Forward result names the peer, whose egress
        // queue carries the adjacency EID.
        match self.rib.find(&bundle) {
            Some(routing::DispatchAction::Drop(reason)) => {
                if let Some(reason) = reason {
                    debug!("Routing lookup indicates bundle should be dropped: {reason:?}");
                    self.drop_bundle(bundle, reason).await
                } else {
                    debug!("Routing lookup indicates bundle should be dropped without reason");
                    self.delete_bundle(bundle).await
                }
            }
            Some(routing::DispatchAction::AdminEndpoint) => {
                self.administrative_bundle(bundle).await
            }
            Some(routing::DispatchAction::Deliver(service)) => {
                // Check for reassembly
                if bundle.id().fragment_info.is_some() {
                    // Reassemble the bundle before delivery
                    self.reassemble(bundle).await
                } else {
                    // Queue to the service's delivery channel
                    if let Err(bundle) = service.deliver(bundle).await {
                        // The service unregistered between the RIB lookup and
                        // the send: park for the next registration.
                        debug!("Service delivery queue closed, parking bundle");
                        let service_eid = self
                            .node_ids
                            .resolve_eid(&service.service_id)
                            .unwrap_or_else(|_| bundle.primary().destination.clone());
                        self.park_bundle(
                            bundle,
                            bundle::BundleStatus::WaitingForService {
                                service: service_eid,
                            },
                            &seen,
                        )
                        .await
                    }
                }
            }
            Some(routing::DispatchAction::Forward { peer, next_hop }) => {
                debug!("Queuing bundle for forwarding to CLA peer {peer}");
                if let Err(bundle) = cla_registry.forward(peer, next_hop, bundle).await {
                    // The peer vanished between the RIB lookup and the
                    // forward: return the bundle to Waiting so the next route
                    // event re-dispatches it, rather than leaving it stranded
                    // in Dispatching/ForwardPending until lifetime expiry.
                    debug!("CLA forward failed, returning bundle to Waiting");
                    self.park_bundle(bundle, bundle::BundleStatus::Waiting, &seen)
                        .await
                }
            }
            None => {
                // No opportunity available — wait for one. A local
                // destination waits for its service to register; anything
                // else waits for a route.
                debug!("Storing bundle until a forwarding opportunity arises");

                let parked = match self
                    .node_ids
                    .local_service_eid(&bundle.primary().destination)
                {
                    Some(service) => bundle::BundleStatus::WaitingForService { service },
                    None => bundle::BundleStatus::Waiting,
                };
                self.park_bundle(bundle, parked, &seen).await
            }
        }
    }

    pub async fn poll_waiting(self: &Arc<Self>, cancel_token: hardy_async::CancellationToken) {
        let (stream, rx) = hardy_async::channel::bounded::<bundle::Bundle>(self.poll_channel_depth);

        let dispatcher = self.clone();

        // Run producer and consumer concurrently
        join!(
            // Producer: feed bundles into the channel until exhausted or
            // cancelled. Racing the poll against cancel (then dropping the
            // stream) stops the producer blocking forever on a full channel
            // after the consumer breaks on cancel — join! keeps the receiver
            // alive, so without this the two sides deadlock.
            async {
                select_biased! {
                    _ = self.store.poll_waiting(&stream).fuse() => {}
                    _ = cancel_token.cancelled().fuse() => {}
                }
                drop(stream);
            },
            // Consumer: drain channel into shared processing pool
            async {
                loop {
                    select_biased! {
                        bundle = rx.recv().fuse() => {
                            let Ok(bundle) = bundle else {
                                break;
                            };

                            let dispatcher = dispatcher.clone();
                            hardy_async::spawn!(self.processing_pool, "poll_waiting_dispatcher", async move {
                                dispatcher.process_bundle(bundle, dispatcher.cla_registry()).await
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
