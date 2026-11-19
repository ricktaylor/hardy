use futures::{FutureExt, join, select_biased};

use super::*;

impl Dispatcher {
    /// Queues a bundle for dispatch processing.
    ///
    /// The caller must have claimed the bundle, at `Dispatching`, or be
    /// re-queueing one recovered still queued, at `DispatchPending`. The
    /// send's conditional swap moves it to `DispatchPending`, the queue's
    /// commit point, until the consumer claims it back.
    pub(super) async fn dispatch_bundle(&self, bundle: bundle::Bundle) {
        debug_assert!(matches!(
            bundle.status,
            bundle::BundleStatus::Dispatching | bundle::BundleStatus::DispatchPending
        ));

        if self.dispatch_tx.send(bundle).await.is_err() {
            debug!("Dispatch queue closed, bundle dropped");
        }
    }

    /// Consumes the dispatch queue, claiming each bundle and processing it
    /// on the pool.
    pub(super) async fn run_dispatch_queue(
        self: Arc<Self>,
        dispatch_rx: hardy_async::closeable::Receiver<bundle::Bundle>,
    ) {
        while let Ok(bundle) = dispatch_rx.recv().await {
            let dispatcher = self.clone();
            hardy_async::spawn!(self.processing_pool, "process_bundle", async move {
                let mut bundle = bundle;
                // Claim the bundle out of `DispatchPending`. The channel
                // delivers at least once, its storage poller being free to
                // push a copy snapshotted before this claim, so a copy that
                // loses the swap is a duplicate and is dropped here. The
                // claim runs inside the pooled task so that claims for
                // successive bundles overlap, the pool permit being the
                // consumer's only await.
                if !dispatcher
                    .store
                    .swap_status(&mut bundle, &bundle::BundleStatus::Dispatching)
                    .await
                {
                    debug!("Bundle already claimed for processing, dropping duplicate copy");
                    return;
                }

                dispatcher.process_bundle(bundle, dispatcher.clas()).await;
            })
            .await;
        }

        debug!("Dispatch queue consumer exiting");
    }

    /// Looks the bundle up in the RIB and executes the decision.
    ///
    /// Bundle data is loaded lazily, only the `AdminEndpoint` and `Deliver`
    /// paths needing it at once. `Forward` defers the load to
    /// `forward_bundle`, past the dequeue from the peer's backpressure
    /// channel.
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
    /// | `None` (local lookup EID, default table) | Wait for a service | `Dispatching` → `WaitingForService` |
    /// | `None` (otherwise) | Wait for route | `Dispatching` → `Waiting` |
    ///
    /// The lookup EID is the bundle's route key, or its destination where
    /// there is none, and a table other than the default waits for a route
    /// whatever the EID.
    ///
    /// See [Routing Design](../../docs/routing_subsystem_design.md) for RIB lookup details.
    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.id())))]
    pub(super) async fn process_bundle(&self, bundle: bundle::Bundle, clas: &Table) {
        // The decision carries the table it was made against, which the
        // parks re-check to close the window against a poll (see
        // `park_bundle`). A `Forward` result names the peer, whose egress
        // queue carries the adjacency.
        let decision = self.rib.decide(&bundle);
        self.execute_dispatch_action(bundle, decision, clas).await
    }

    // Executes an arrival's routing decision of record, after the metadata
    // insert, on the receiving task. The decision was made at the pre-drain
    // gate, so it is made again if the table changed during the drain: a
    // route withdrawn meanwhile must not carry the bundle. That narrows the
    // window to the one dispatch always has, from here to the hand-off,
    // without closing it.
    //
    // The arms whose cost scales with embedder callbacks or the payload, the
    // admin endpoint and a fragment's reassembly, which may complete the
    // ADU, go through the dispatch queue to the processing pool, so the
    // caller, a CLA session awaiting its acknowledgement, waits on nothing
    // heavier than a status swap and a queue push. The queue send never
    // awaits a pool permit, so a pool task may call this too.
    pub(super) async fn execute_admitted(
        &self,
        bundle: bundle::Bundle,
        mut decision: routing::Decision,
    ) {
        if self.rib.table_changed_since(&decision.seen) {
            debug!("Routing changed during the drain, deciding again");
            decision = self.rib.decide(&bundle);
        }

        match decision.action {
            Some(routing::DispatchAction::AdminEndpoint) => self.dispatch_bundle(bundle).await,
            Some(routing::DispatchAction::Deliver(_)) if bundle.id().fragment_info.is_some() => {
                self.dispatch_bundle(bundle).await
            }
            _ => {
                self.execute_dispatch_action(bundle, decision, self.clas())
                    .await
            }
        }
    }

    // Executes a routing decision, for `process_bundle`, which routes at
    // dispatch on the re-dispatch, poll, sweep and recovery paths, and for
    // `execute_admitted`, with an arrival's decision of record. A decision
    // that proves stale at the hand-off lands in a failure arm whose park
    // re-checks the table it was made against and re-enters dispatch, and
    // one the hand-off accepts stands, so callers keep the window between
    // the decision and this call short.
    pub(super) async fn execute_dispatch_action(
        &self,
        bundle: bundle::Bundle,
        decision: routing::Decision,
        clas: &Table,
    ) {
        let routing::Decision { action, seen } = decision;

        // The reaper defers the hand-off statuses, `DeliveryAckPending` and
        // `ForwardAckPending`, so an expired bundle can re-enter dispatch
        // through a transfer outcome, a sweep or a poll, and the ingress
        // drain takes real time. Expiry is resolved here rather than routing
        // the bundle onward.
        if bundle.has_expired() {
            return self.drop_bundle(bundle, ReasonCode::LifetimeExpired).await;
        }

        match action {
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
                if bundle.id().fragment_info.is_some() {
                    self.reassemble(bundle).await
                } else {
                    if let Err(bundle) = service.deliver(bundle).await {
                        // The service unregistered between the RIB lookup
                        // and the send, so the bundle parks for the next
                        // registration, under the canonical registration EID
                        // stored at construction, the key the
                        // re-registration poll matches.
                        debug!("Service delivery queue closed, parking bundle");
                        self.park_bundle(
                            bundle,
                            bundle::BundleStatus::WaitingForService {
                                service: service.eid().clone(),
                            },
                            &seen,
                        )
                        .await
                    }
                }
            }
            Some(routing::DispatchAction::Forward { peer, next_hop }) => {
                debug!("Queuing bundle for forwarding to CLA peer {peer}");
                if let Err(bundle) = clas.forward(peer, next_hop, bundle).await {
                    // The peer vanished between the RIB lookup and the
                    // forward, so the bundle returns to `Waiting` and the
                    // next routing event re-dispatches it, rather than
                    // stranding in `Dispatching` or `ForwardPending` until
                    // lifetime expiry.
                    debug!("CLA forward failed, returning bundle to Waiting");
                    self.park_bundle(bundle, bundle::BundleStatus::Waiting, &seen)
                        .await
                }
            }
            None => {
                // No opportunity is available, so the bundle waits for one
                // under the inputs the lookup walked. A local lookup EID,
                // the key or else the destination, waits for its service to
                // register, and anything else waits for a route, a table
                // other than the default included, which no registration can
                // satisfy.
                debug!("Storing bundle until a forwarding opportunity arises");

                let key = bundle.metadata.routing_key();
                let service = match key.unknown_table() {
                    Some(_) => None,
                    None => self
                        .node_ids
                        .local_service_eid(key.lookup_eid(&bundle.primary().destination)),
                };
                let parked = match service {
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

        join!(
            // The producer feeds bundles into the channel until it is
            // exhausted or cancelled. Racing the poll against the token and
            // then dropping the stream stops the producer blocking forever
            // on a full channel after the consumer breaks on cancellation:
            // `join!` keeps the receiver alive, so without this the two
            // sides deadlock.
            async {
                select_biased! {
                    _ = self.store.poll_waiting(&stream).fuse() => {}
                    _ = cancel_token.cancelled().fuse() => {}
                }
                drop(stream);
            },
            // The consumer drains the channel into the processing pool.
            async {
                loop {
                    select_biased! {
                        bundle = rx.recv().fuse() => {
                            let Ok(bundle) = bundle else {
                                break;
                            };

                            let dispatcher = dispatcher.clone();
                            hardy_async::spawn!(self.processing_pool, "poll_waiting_dispatcher", async move {
                                dispatcher.process_bundle(bundle, dispatcher.clas()).await
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
