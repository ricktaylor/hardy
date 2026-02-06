use super::*;
use futures::{FutureExt, join, select_biased};
use hardy_bpv7::status_report::ReasonCode;

impl Dispatcher {
    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    pub async fn receive_bundle(
        self: &Arc<Self>,
        mut data: Bytes,
        ingress_cla: Option<Arc<str>>,
        ingress_peer_node: Option<hardy_bpv7::eid::NodeId>,
        ingress_peer_addr: Option<cla::ClaAddress>,
    ) -> cla::Result<()> {
        // TODO: Really should not return errors when the bundle content is garbage - it's not the CLAs responsibility to fix it!

        // Capture received_at as soon as possible
        let received_at = time::OffsetDateTime::now_utc();

        // Do a fast pre-check
        match data.first() {
            None => {
                return Err(hardy_bpv7::Error::InvalidCBOR(
                    hardy_cbor::decode::Error::NeedMoreData(1),
                )
                .into());
            }
            Some(0x06) => {
                debug!("Data looks like a BPv6 bundle");
                return Err(hardy_bpv7::Error::InvalidCBOR(
                    hardy_cbor::decode::Error::IncorrectType(
                        "BPv7 bundle".to_string(),
                        "Possible BPv6 bundle".to_string(),
                    ),
                )
                .into());
            }
            Some(0x80..=0x9F) => {}
            _ => {
                return Err(hardy_bpv7::Error::InvalidCBOR(
                    hardy_cbor::decode::Error::IncorrectType(
                        "BPv7 bundle".to_string(),
                        "Invalid CBOR".to_string(),
                    ),
                )
                .into());
            }
        }

        // Parse the bundle
        let (bundle, reason, report_unsupported) =
            match hardy_bpv7::bundle::RewrittenBundle::parse(&data, self.key_provider())? {
                hardy_bpv7::bundle::RewrittenBundle::Valid {
                    bundle,
                    report_unsupported,
                } => (
                    bundle::Bundle {
                        metadata: BundleMetadata {
                            storage_name: Some(self.store.save_data(&data).await),
                            received_at,
                            ingress_cla: ingress_cla.clone(),
                            ingress_peer_node: ingress_peer_node.clone(),
                            ingress_peer_addr: ingress_peer_addr.clone(),
                            ..Default::default()
                        },
                        bundle,
                    },
                    None,
                    report_unsupported,
                ),
                hardy_bpv7::bundle::RewrittenBundle::Rewritten {
                    bundle,
                    new_data,
                    report_unsupported,
                    non_canonical,
                } => {
                    debug!("Received bundle has been rewritten");

                    data = Bytes::from(new_data);
                    let storage_name = Some(self.store.save_data(&data).await);

                    (
                        bundle::Bundle {
                            metadata: BundleMetadata {
                                storage_name,
                                received_at,
                                ingress_cla: ingress_cla.clone(),
                                ingress_peer_node: ingress_peer_node.clone(),
                                ingress_peer_addr: ingress_peer_addr.clone(),
                                non_canonical,
                                ..Default::default()
                            },
                            bundle,
                        },
                        None,
                        report_unsupported,
                    )
                }
                hardy_bpv7::bundle::RewrittenBundle::Invalid {
                    bundle,
                    reason,
                    error,
                } => {
                    debug!("Invalid bundle received: {error}");

                    // Don't bother saving the bundle data, it's garbage
                    (
                        bundle::Bundle {
                            metadata: BundleMetadata {
                                received_at,
                                ingress_cla,
                                ingress_peer_node,
                                ingress_peer_addr,
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
            if report_unsupported {
                ReasonCode::BlockUnsupported
            } else {
                ReasonCode::NoAdditionalInformation
            },
        )
        .await;

        if reason.is_some() {
            // Not valid, drop it
            self.drop_bundle(bundle, reason).await;
        } else {
            // Spawn into processing pool for rate limiting
            self.ingest_bundle(bundle, data).await;
        }
        Ok(())
    }

    /// Spawn bundle ingestion into the processing pool for rate limiting
    pub(super) async fn ingest_bundle(self: &Arc<Self>, bundle: bundle::Bundle, data: Bytes) {
        let dispatcher = self.clone();
        hardy_async::spawn!(self.processing_pool, "ingest_bundle", async move {
            dispatcher.ingest_bundle_inner(bundle, data).await
        })
        .await;
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    pub(super) async fn ingest_bundle_inner(&self, mut bundle: bundle::Bundle, mut data: Bytes) {
        if let Some(u) = bundle.bundle.flags.unrecognised {
            debug!("Bundle primary block has unrecognised flag bits set: {u:#x}");
        }

        // Check lifetime first
        if bundle.has_expired() {
            debug!("Bundle lifetime has expired");
            return self
                .drop_bundle(bundle, Some(ReasonCode::LifetimeExpired))
                .await;
        }

        // Check hop count exceeded
        if let Some(hop_info) = bundle.bundle.hop_count.as_ref()
            && hop_info.count > hop_info.limit
        {
            debug!("Bundle hop-limit {} exceeded", hop_info.limit);
            return self
                .drop_bundle(bundle, Some(ReasonCode::HopLimitExceeded))
                .await;
        }

        // Ingress filter hook
        (bundle, data) = match self
            .filter_registry
            .exec(
                filters::Hook::Ingress,
                bundle,
                data,
                self.key_provider(),
                &self.processing_pool,
            )
            .await
            .trace_expect("Ingress filter execution failed")
        {
            filters::registry::ExecResult::Continue(mutation, mut bundle, data) => {
                // Persist any bundle data mutations
                if mutation.bundle {
                    let new_storage_name = self.store.save_data(&data).await;
                    if let Some(old_storage_name) = bundle.metadata.storage_name.take() {
                        self.store.delete_data(&old_storage_name).await;
                    }
                    bundle.metadata.storage_name = Some(new_storage_name);
                }
                // Always checkpoint to Dispatching (crash safety)
                bundle.metadata.status = BundleStatus::Dispatching;
                self.store.update_metadata(&bundle).await;
                (bundle, data)
            }
            filters::registry::ExecResult::Drop(bundle, reason) => {
                return self.drop_bundle(bundle, reason).await;
            }
        };

        self.process_bundle(bundle, data).await;
    }

    /// Queue a bundle for dispatch processing
    pub(super) async fn dispatch_bundle(&self, mut bundle: bundle::Bundle) {
        if bundle.metadata.status != BundleStatus::Dispatching {
            bundle.metadata.status = BundleStatus::Dispatching;
            self.store.update_metadata(&bundle).await;
        }

        if self
            .dispatch_tx
            .get()
            .trace_expect("Dispatcher not started")
            .send(bundle)
            .await
            .is_err()
        {
            debug!("Dispatch queue closed, bundle dropped");
        }
    }

    /// Consumer task for the dispatch queue
    pub(super) async fn run_dispatch_queue(
        self: Arc<Self>,
        dispatch_rx: storage::channel::Receiver,
    ) {
        while let Ok(bundle) = dispatch_rx.recv_async().await {
            if bundle.has_expired() {
                debug!("Bundle lifetime has expired while queued");
                self.drop_bundle(bundle, Some(ReasonCode::LifetimeExpired))
                    .await;
                continue;
            }

            let dispatcher = self.clone();
            hardy_async::spawn!(self.processing_pool, "process_bundle", async move {
                if let Some(data) = dispatcher.load_data(&bundle).await {
                    dispatcher.process_bundle(bundle, data).await;
                } else {
                    // Bundle data was deleted while queued
                    dispatcher.drop_bundle(bundle, None).await;
                }
            })
            .await;
        }

        debug!("Dispatch queue consumer exiting");
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    async fn process_bundle(&self, mut bundle: bundle::Bundle, data: Bytes) {
        // Perform RIB lookup (sets bundle.metadata.next_hop for Forward results)
        match self.rib.find(&bundle.bundle, &mut bundle.metadata) {
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
                self.cla_registry.forward(peer, bundle).await
            }
            _ => {
                // No route available - wait for one
                debug!("Storing bundle until a forwarding opportunity arises");

                if bundle.metadata.status != BundleStatus::Waiting {
                    bundle.metadata.status = BundleStatus::Waiting;
                    self.store.update_metadata(&bundle).await;
                }
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
                                    dispatcher.process_bundle(bundle, data).await
                                } else {
                                    // Bundle data was deleted sometime while we waited, drop the bundle
                                    dispatcher.drop_bundle(bundle, None).await
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
