use super::*;
use hardy_cbor as cbor;
use hardy_proto::application::*;
use tokio::sync::mpsc::*;

#[derive(Clone)]
struct Config {
    node_id: node_id::NodeId,
    status_reports: bool,
    max_forwarding_delay: u32,
}

impl Config {
    fn load(config: &config::Config, node_id: node_id::NodeId) -> Result<Self, anyhow::Error> {
        Ok(Self {
            node_id,
            status_reports: settings::get_with_default(config, "status_reports", false)?,
            max_forwarding_delay: settings::get_with_default(config, "max_forwarding_delay", 5u32)?,
        })
    }
}

#[derive(Clone)]
pub struct Dispatcher {
    config: Config,
    store: store::Store,
    tx: Sender<(bundle::Metadata, bundle::Bundle)>,
    cla_registry: cla_registry::ClaRegistry,
    app_registry: app_registry::AppRegistry,
    fib: Option<fib::Fib>,
}

impl Dispatcher {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: &config::Config,
        node_id: node_id::NodeId,
        store: store::Store,
        cla_registry: cla_registry::ClaRegistry,
        app_registry: app_registry::AppRegistry,
        fib: Option<fib::Fib>,
        task_set: &mut tokio::task::JoinSet<()>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<Self, anyhow::Error> {
        // Load config
        let config = Config::load(config, node_id)?;

        if !config.status_reports {
            log::info!("Bundle status reports are disabled by configuration");
        }

        if config.max_forwarding_delay == 0 {
            log::info!("Forwarding synchronization delay disabled by configuration");
        }

        // Create a channel for bundles
        let (tx, rx) = channel(16);
        let dispatcher = Self {
            config,
            store,
            tx,
            cla_registry,
            app_registry,
            fib,
        };

        // Spawn a bundle receiver
        let dispatcher_cloned = dispatcher.clone();
        task_set
            .spawn(async move { Self::pipeline_pump(dispatcher_cloned, rx, cancel_token).await });

        Ok(dispatcher)
    }

    async fn enqueue_bundle(
        &self,
        metadata: bundle::Metadata,
        bundle: bundle::Bundle,
    ) -> Result<(), anyhow::Error> {
        // Put bundle into channel
        self.tx.send((metadata, bundle)).await.map_err(|e| e.into())
    }

    async fn pipeline_pump(
        self,
        mut rx: Receiver<(bundle::Metadata, bundle::Bundle)>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        // We're going to spawn a bunch of tasks
        let mut task_set = tokio::task::JoinSet::new();

        loop {
            tokio::select! {
                bundle = rx.recv() => match bundle {
                    None => break,
                    Some((metadata,bundle)) => {
                        let dispatcher = self.clone();
                        task_set.spawn(async move {
                            dispatcher.process_bundle(metadata,bundle).await.log_expect("Failed to process bundle");
                        });
                    }
                },
                Some(r) = task_set.join_next() => r.log_expect("Task terminated unexpectedly"),
                _ = cancel_token.cancelled() => break
            }
        }

        // Wait for all sub-tasks to complete
        while let Some(r) = task_set.join_next().await {
            r.log_expect("Task terminated unexpectedly")
        }
    }

    #[instrument(skip(self))]
    pub async fn process_bundle(
        &self,
        mut metadata: bundle::Metadata,
        mut bundle: bundle::Bundle,
    ) -> Result<(), anyhow::Error> {
        if let bundle::BundleStatus::DispatchPending = &metadata.status {
            // Check if we are the final destination
            let new_status = if self.config.node_id.is_local_service(&bundle.destination) {
                if bundle.id.fragment_info.is_some() {
                    // Reassembly!!
                    bundle::BundleStatus::ReassemblyPending
                } else {
                    // The bundle is ready for collection
                    bundle::BundleStatus::CollectionPending
                }
            } else {
                // Forward to another BPA
                bundle::BundleStatus::ForwardPending
            };
            metadata.status = self
                .store
                .set_status(&metadata.storage_name, new_status)
                .await?;
        }

        if let bundle::BundleStatus::ForwardPending = &metadata.status {
            return self.forward_bundle(metadata, bundle).await;
        }

        if let bundle::BundleStatus::ReassemblyPending = &metadata.status {
            // Attempt reassembly
            let Some((m, b)) = self.reassemble(metadata, bundle).await? else {
                // Waiting for more fragments to arrive
                return Ok(());
            };
            (metadata, bundle) = (m, b);
        }

        if let bundle::BundleStatus::CollectionPending = &metadata.status {
            // Check if we have a local service registered
            if let Some(endpoint) = self.app_registry.find_by_eid(&bundle.destination) {
                // Notify that the bundle is ready
                endpoint.collection_notify(&bundle.id).await;
            }
        }
        Ok(())
    }

    #[instrument(skip(self))]
    async fn forward_bundle(
        &self,
        metadata: bundle::Metadata,
        bundle: bundle::Bundle,
    ) -> Result<(), anyhow::Error> {
        let Some(fib) = &self.fib else {
            /* If forwarding is disabled in the configuration, then we can only deliver bundles.
             * As we have decided that the bundle is not for a local service, we cannot deliver.
             * Therefore, we respond with a Destination endpoint ID unavailable report */
            return self
                .drop_bundle(
                    metadata,
                    bundle,
                    Some(bundle::StatusReportReasonCode::DestinationEndpointIDUnavailable),
                )
                .await;
        };

        // Resolve destination
        let Ok(mut destination) = bundle.destination.clone().try_into() else {
            // Bundle destination is not a valid next-hop
            return self
                .drop_bundle(
                    metadata,
                    bundle,
                    Some(bundle::StatusReportReasonCode::DestinationEndpointIDUnavailable),
                )
                .await;
        };

        /* We loop here, as the FIB could tell us that there should be a CLA to use to forward
         * But it might be rebooting or jammed, so we keep retrying for a "reasonable" amount of time */
        let mut data = None;
        let mut previous = false;
        let mut retries = 0;
        let mut actions = fib.find(&destination).into_iter();
        let reason = loop {
            // Lookup/Perform actions
            match actions.next() {
                Some(fib::ForwardAction::Drop(reason)) => break reason,
                Some(fib::ForwardAction::Wait) => todo!(),
                Some(fib::ForwardAction::Forward(a)) => {
                    if retries > self.config.max_forwarding_delay {
                        // We have delayed long enough trying to forward
                        break Some(
                            bundle::StatusReportReasonCode::NoTimelyContactWithNextNodeOnRoute,
                        );
                    }

                    // Find the named CLA
                    if let Some(endpoint) = self.cla_registry.find_by_name(&a.name) {
                        // Get bundle data from store
                        if data.is_none() {
                            data = match self.store.load_data(&metadata.storage_name).await {
                                Ok(data) => Some((*data).as_ref().to_vec()),
                                Err(e) => {
                                    // The bundle data has gone!
                                    log::warn!("Failed to load bundle data: {}", e);
                                    return self
                                        .drop_bundle(
                                            metadata,
                                            bundle,
                                            Some(bundle::StatusReportReasonCode::DepletedStorage),
                                        )
                                        .await;
                                }
                            };
                        }

                        if endpoint
                            .forward_bundle(a.address.clone(), data.clone().unwrap())
                            .await
                            .inspect_err(|e| log::warn!("{}", e))
                            .is_ok()
                        {
                            // We have successfully forwarded!
                            return Ok(());
                        }
                    }
                }
                None => {
                    if retries > self.config.max_forwarding_delay {
                        if previous {
                            // We have delayed long enough trying to find a route to previous_node
                            break Some(
                                bundle::StatusReportReasonCode::NoKnownRouteToDestinationFromHere,
                            );
                        }

                        // Return the bundle to the source via the 'previous_node' or 'bundle.source'
                        if let Ok(previous_node) = bundle
                            .previous_node
                            .clone()
                            .unwrap_or(bundle.id.source.clone())
                            .try_into()
                        {
                            // Try the previous_node
                            destination = previous_node;
                        } else {
                            // Previous node is not a valid next-hop
                            break Some(
                                bundle::StatusReportReasonCode::DestinationEndpointIDUnavailable,
                            );
                        }

                        // Reset retry counter as we are attempting to return the bundle
                        previous = true;
                        retries = 0;
                    } else {
                        // Async sleep for 1 second
                        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
                        retries = retries.saturating_add(1);
                    }

                    // Lookup again
                    actions = fib.find(&destination).into_iter();
                }
            }
        };

        self.drop_bundle(metadata, bundle, reason).await
    }

    #[instrument(skip(self))]
    async fn drop_bundle(
        &self,
        metadata: bundle::Metadata,
        bundle: bundle::Bundle,
        reason: Option<bundle::StatusReportReasonCode>,
    ) -> Result<(), anyhow::Error> {
        if let Some(reason) = reason {
            self.report_bundle_deletion(&metadata, &bundle, reason)
                .await?;
        }
        self.store.remove(&metadata.storage_name).await
    }

    #[instrument(skip(self))]
    pub async fn report_bundle_reception(
        &self,
        metadata: &bundle::Metadata,
        bundle: &bundle::Bundle,
        reason: bundle::StatusReportReasonCode,
    ) -> Result<(), anyhow::Error> {
        // Check if a report is requested
        if !self.config.status_reports || !bundle.flags.receipt_report_requested {
            return Ok(());
        }

        // Create a bundle report
        let (metadata, bundle) = bundle::Builder::new(bundle::BundleStatus::DispatchPending)
            .is_admin_record(true)
            .source(&self.config.node_id.get_admin_endpoint(&bundle.report_to))
            .destination(&bundle.report_to)
            .add_payload_block(new_bundle_status_report(
                metadata, bundle, reason, None, None, None,
            ))
            .build(&self.store)
            .await?;

        // And queue it up
        self.enqueue_bundle(metadata, bundle).await
    }

    #[instrument(skip(self))]
    pub async fn report_bundle_deletion(
        &self,
        metadata: &bundle::Metadata,
        bundle: &bundle::Bundle,
        reason: bundle::StatusReportReasonCode,
    ) -> Result<(), anyhow::Error> {
        // Check if a report is requested
        if !self.config.status_reports || !bundle.flags.delete_report_requested {
            return Ok(());
        }

        // Create a bundle report
        let (metadata, bundle) = bundle::Builder::new(bundle::BundleStatus::DispatchPending)
            .is_admin_record(true)
            .source(&self.config.node_id.get_admin_endpoint(&bundle.report_to))
            .destination(&bundle.report_to)
            .add_payload_block(new_bundle_status_report(
                metadata,
                bundle,
                reason,
                None,
                None,
                Some(time::OffsetDateTime::now_utc()),
            ))
            .build(&self.store)
            .await?;

        // And queue it up
        self.enqueue_bundle(metadata, bundle).await
    }

    #[instrument(skip(self))]
    pub async fn local_dispatch(
        &self,
        source: bundle::Eid,
        request: SendRequest,
    ) -> Result<(), anyhow::Error> {
        // Build the bundle
        let destination = match request.destination.parse::<bundle::Eid>()? {
            bundle::Eid::Null => return Err(anyhow!("Cannot send to Null endpoint")),
            eid => eid,
        };

        let mut b = bundle::Builder::new(bundle::BundleStatus::DispatchPending)
            .source(&source)
            .destination(&destination);

        // Set flags
        if let Some(flags) = request.flags {
            if flags & (send_request::SendFlags::Acknowledge as u32) != 0 {
                b = b.app_ack_requested(true);
            }
            if flags & (send_request::SendFlags::DoNotFragment as u32) != 0 {
                b = b.do_not_fragment(true)
            }
            b = b.report_to(&self.config.node_id.get_admin_endpoint(&destination));
        }

        // Lifetime
        if let Some(lifetime) = request.lifetime {
            b = b.lifetime(lifetime);
        }

        // Add payload and build
        let (metadata, bundle) = b.add_payload_block(request.data).build(&self.store).await?;

        // And queue it up
        self.enqueue_bundle(metadata, bundle).await
    }

    async fn reassemble(
        &self,
        _metadata: bundle::Metadata,
        _bundle: bundle::Bundle,
    ) -> Result<Option<(bundle::Metadata, bundle::Bundle)>, anyhow::Error> {
        todo!()
    }
}

fn new_bundle_status_report(
    metadata: &bundle::Metadata,
    bundle: &bundle::Bundle,
    reason: bundle::StatusReportReasonCode,
    forwarded: Option<time::OffsetDateTime>,
    delivered: Option<time::OffsetDateTime>,
    deleted: Option<time::OffsetDateTime>,
) -> Vec<u8> {
    cbor::encode::emit_array(Some(2), |a| {
        a.emit(1);
        a.emit_array(Some(bundle.id.fragment_info.map_or(4, |_| 6)), |a| {
            // Statuses
            a.emit_array(Some(4), |a| {
                // Report node received bundle
                match metadata.received_at {
                    Some(received_at)
                        if bundle.flags.report_status_time
                            && bundle.flags.receipt_report_requested =>
                    {
                        a.emit_array(Some(2), |a| {
                            a.emit(true);
                            a.emit(bundle::as_dtn_time(&received_at))
                        })
                    }
                    _ => a.emit_array(Some(1), |a| a.emit(bundle.flags.receipt_report_requested)),
                }

                // Report node forwarded the bundle
                match forwarded {
                    Some(forwarded)
                        if bundle.flags.report_status_time
                            && bundle.flags.forward_report_requested =>
                    {
                        a.emit_array(Some(2), |a| {
                            a.emit(true);
                            a.emit(bundle::as_dtn_time(&forwarded))
                        })
                    }
                    Some(_) => {
                        a.emit_array(Some(1), |a| a.emit(bundle.flags.forward_report_requested))
                    }
                    _ => a.emit_array(Some(1), |a| a.emit(false)),
                }

                // Report node delivered the bundle
                match delivered {
                    Some(delivered)
                        if bundle.flags.report_status_time
                            && bundle.flags.delivery_report_requested =>
                    {
                        a.emit_array(Some(2), |a| {
                            a.emit(true);
                            a.emit(bundle::as_dtn_time(&delivered))
                        })
                    }
                    Some(_) => {
                        a.emit_array(Some(1), |a| a.emit(bundle.flags.delivery_report_requested))
                    }
                    _ => a.emit_array(Some(1), |a| a.emit(false)),
                }

                // Report node deleted the bundle
                match deleted {
                    Some(deleted)
                        if bundle.flags.report_status_time
                            && bundle.flags.delete_report_requested =>
                    {
                        a.emit_array(Some(2), |a| {
                            a.emit(true);
                            a.emit(bundle::as_dtn_time(&deleted))
                        })
                    }
                    Some(_) => {
                        a.emit_array(Some(1), |a| a.emit(bundle.flags.delete_report_requested))
                    }
                    _ => a.emit_array(Some(1), |a| a.emit(false)),
                }
            });

            // Reason code
            a.emit(reason);
            // Source EID
            a.emit(&bundle.id.source);
            // Creation Timestamp
            a.emit(&bundle.id.timestamp);

            if let Some(fragment_info) = &bundle.id.fragment_info {
                // Add fragment info
                a.emit(fragment_info.offset);
                a.emit(fragment_info.total_len);
            }
        })
    })
}
