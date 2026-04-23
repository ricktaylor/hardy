use super::*;
use futures::{FutureExt, join, select_biased};
use hardy_bpv7::status_report::ReasonCode;

impl Dispatcher {
    /// Entry point for bundles received from CLAs.
    ///
    /// Parses the CBOR-encoded bundle, validates the format, stores bundle data,
    /// inserts initial metadata with `New` status, and queues for ingestion.
    ///
    /// # Bundle State
    ///
    /// - Initial status: `New`
    /// - Next: `ingest_bundle()` → Ingress filter → `Dispatching`
    ///
    /// See [Bundle State Machine Design](../../docs/bundle_state_machine_design.md)
    /// for the complete state transition diagram.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn receive_bundle(
        self: &Arc<Self>,
        mut data: Bytes,
        ingress_cla: Option<Arc<str>>,
        ingress_peer_node: Option<hardy_bpv7::eid::NodeId>,
        ingress_peer_addr: Option<cla::ClaAddress>,
    ) -> cla::Result<()> {
        // TODO: Really should not return errors when the bundle content is garbage - it's not the CLAs responsibility to fix it!

        // Count every bundle received from a CLA, before any validation
        metrics::counter!("bpa.bundle.received").increment(1);
        metrics::counter!("bpa.bundle.received.bytes").increment(data.len() as u64);

        // Capture received_at as soon as possible
        let received_at = time::OffsetDateTime::now_utc();

        // Fast pre-check: reject empty, BPv6, and non-CBOR-array data
        if let Err(e) = crate::cbor::precheck(&data) {
            metrics::counter!("bpa.bundle.received.dropped").increment(1);
            return Err(e.into());
        }

        // Parse the bundle
        let (bundle, reason, report_unsupported) =
            match hardy_bpv7::bundle::RewrittenBundle::parse(&data, self.key_provider()) {
                Err(e) => {
                    metrics::counter!("bpa.bundle.received.dropped").increment(1);
                    return Err(e.into());
                }
                Ok(hardy_bpv7::bundle::RewrittenBundle::Valid {
                    bundle,
                    report_unsupported,
                }) => (
                    bundle::Bundle {
                        metadata: bundle::BundleMetadata {
                            storage_name: Some(self.store.save_data(&data).await),
                            read_only: bundle::ReadOnlyMetadata {
                                received_at,
                                ingress_peer_node,
                                ingress_peer_addr,
                                ingress_cla,
                                ..Default::default()
                            },
                            ..Default::default()
                        },
                        bundle,
                    },
                    None,
                    report_unsupported,
                ),
                Ok(hardy_bpv7::bundle::RewrittenBundle::Rewritten {
                    bundle,
                    new_data,
                    report_unsupported,
                    non_canonical: _,
                }) => {
                    debug!("Received bundle has been rewritten");

                    data = Bytes::from(new_data);
                    let storage_name = Some(self.store.save_data(&data).await);

                    (
                        bundle::Bundle {
                            metadata: bundle::BundleMetadata {
                                storage_name,
                                read_only: bundle::ReadOnlyMetadata {
                                    received_at,
                                    ingress_peer_node,
                                    ingress_peer_addr,
                                    ingress_cla,
                                    ..Default::default()
                                },
                                ..Default::default()
                            },
                            bundle,
                        },
                        None,
                        report_unsupported,
                    )
                }
                Ok(hardy_bpv7::bundle::RewrittenBundle::Invalid {
                    bundle,
                    reason,
                    error,
                }) => {
                    debug!("Invalid bundle received: {error}");

                    // Don't bother saving the bundle data, it's garbage
                    (
                        bundle::Bundle {
                            metadata: bundle::BundleMetadata {
                                read_only: bundle::ReadOnlyMetadata {
                                    received_at,
                                    ingress_peer_node,
                                    ingress_peer_addr,
                                    ingress_cla,
                                    ..Default::default()
                                },
                                ..Default::default()
                            },
                            bundle,
                        },
                        Some(reason),
                        false,
                    )
                }
            };

        if !self.store.insert_metadata(&bundle).await {
            // Bundle with matching id already exists in the metadata store
            metrics::counter!("bpa.bundle.received.duplicate").increment(1);

            // TODO: There may be custody transfer signalling that needs to happen here

            // Drop the stored data and do not process further
            if let Some(storage_name) = &bundle.metadata.storage_name {
                self.store.delete_data(storage_name).await;
            }
            return Ok(());
        }

        // Report we have received the bundle
        self.report_bundle_reception(
            &bundle,
            if let Some(reason) = &reason {
                *reason
            } else if report_unsupported {
                ReasonCode::BlockUnsupported
            } else {
                ReasonCode::NoAdditionalInformation
            },
        )
        .await;

        if reason.is_some() {
            // Invalid bundle — never entered the pipeline, just clean up
            self.store.tombstone_metadata(&bundle.bundle.id).await;
            metrics::counter!("bpa.bundle.received.dropped").increment(1);
        } else {
            // Spawn into processing pool for rate limiting
            self.ingest_bundle(bundle, data).await;
        }
        Ok(())
    }

    /// Spawn bundle ingestion into the processing pool for rate limiting.
    ///
    /// This is a rate-limiting wrapper that spawns `ingest_bundle_inner()` into
    /// the bounded processing pool. The function returns once the task *starts*,
    /// not when it completes.
    ///
    /// # Crash Safety
    ///
    /// Because this returns before the Ingress filter completes, bundles remain
    /// in `New` status until `ingest_bundle_inner()` checkpoints to `Dispatching`.
    pub(super) async fn ingest_bundle(self: &Arc<Self>, bundle: bundle::Bundle, data: Bytes) {
        metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&bundle.metadata.status)).increment(1.0);

        let dispatcher = self.clone();
        hardy_async::spawn!(self.processing_pool, "ingest_bundle", async move {
            dispatcher.ingest_bundle_inner(bundle, data).await
        })
        .await;
    }

    /// Core bundle ingestion logic: validation, Ingress filter, and checkpoint.
    ///
    /// # Processing Steps
    ///
    /// 1. Validate lifetime (drop if expired)
    /// 2. Validate hop count (drop if exceeded)
    /// 3. Execute Ingress filter hook
    /// 4. Persist any filter mutations (crash-safe ordering)
    /// 5. **Checkpoint**: Transition status to `Dispatching`
    /// 6. Call `process_bundle()` for routing decision
    ///
    /// # Crash Safety
    ///
    /// The checkpoint to `Dispatching` is always persisted after the Ingress
    /// filter completes. On restart, bundles in `New` status re-run from step 1,
    /// while bundles in `Dispatching` skip directly to routing.
    ///
    /// See [Filter Subsystem Design](../../docs/filter_subsystem_design.md) for
    /// filter execution details.
    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    pub(super) async fn ingest_bundle_inner(&self, mut bundle: bundle::Bundle, mut data: Bytes) {
        // Ingress filter hook (includes bundle-validity: flags, lifetime, hop-count)
        (bundle, data) = match self
            .filter_engine
            .exec(
                filter::Hook::Ingress,
                bundle,
                data,
                self.key_provider(),
                &self.processing_pool,
            )
            .await
            // TODO: Replace trace_expect with proper error handling
            .trace_expect("Ingress filter execution failed")
        {
            filter::ExecResult::Continue(mutation, bundle, data) => {
                // Persist filter mutations if any (bundle stays New in storage)
                if mutation.data {
                    if let Some(storage_name) = &bundle.metadata.storage_name {
                        self.store.replace_data(storage_name, &data).await;
                    }
                }
                if mutation.metadata {
                    self.store.update_metadata(&bundle).await;
                }
                (bundle, data)
            }
            filter::ExecResult::Drop(bundle, reason) => {
                return self.drop_bundle(bundle, reason).await;
            }
        };

        self.process_bundle(bundle, data, self.cla_engine()).await;
    }

    /// Queue a bundle for dispatch processing
    pub(super) async fn dispatch_bundle(&self, bundle: bundle::Bundle) {
        if self.dispatch_tx.send(bundle).await.is_err() {
            debug!("Dispatch queue closed, bundle dropped");
        }
    }

    /// Consumer task for the dispatch queue
    pub(super) async fn run_dispatch_queue(self: Arc<Self>, dispatch_rx: storage::Receiver) {
        while let Ok(Some(bundle)) = dispatch_rx.recv_async().await {
            if bundle.has_expired() {
                debug!("Bundle lifetime has expired while queued");
                self.drop_bundle(bundle, Some(ReasonCode::LifetimeExpired))
                    .await;
                continue;
            }

            let dispatcher = self.clone();
            hardy_async::spawn!(self.processing_pool, "process_bundle", async move {
                if let Some(data) = dispatcher.load_data(&bundle).await {
                    dispatcher
                        .process_bundle(bundle, data, dispatcher.cla_engine())
                        .await;
                } else {
                    // Bundle data was deleted while queued
                    dispatcher
                        .drop_bundle(bundle, Some(ReasonCode::DepletedStorage))
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
    async fn process_bundle(
        &self,
        mut bundle: bundle::Bundle,
        data: Bytes,
        cla_engine: &cla::engine::ClaEngine,
    ) {
        // Perform RIB lookup (sets bundle.metadata.next_hop for Forward results)
        match self.rib.find(&mut bundle) {
            Some(rib::FindResult::Drop(reason)) => {
                debug!("Routing lookup indicates bundle should be dropped: {reason:?}");
                self.drop_bundle(bundle, reason).await
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
                if let Err(bundle) = cla_engine.forward(peer, bundle).await {
                    debug!("CLA forward failed, returning bundle to watch queue");
                    self.store.watch_bundle(bundle).await;
                }
            }
            _ => {
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

                            if bundle.has_expired() {
                                debug!("Bundle lifetime has expired");
                                self.drop_bundle(bundle, Some(ReasonCode::LifetimeExpired)).await;
                                continue;
                            }

                            let dispatcher = dispatcher.clone();
                            hardy_async::spawn!(self.processing_pool, "poll_waiting_dispatcher", async move {
                                if let Some(data) = dispatcher.load_data(&bundle).await {
                                    dispatcher.process_bundle(bundle, data, dispatcher.cla_engine()).await
                                } else {
                                    // Bundle data was deleted sometime while we waited, drop the bundle
                                    dispatcher.drop_bundle(bundle, Some(ReasonCode::DepletedStorage)).await
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
