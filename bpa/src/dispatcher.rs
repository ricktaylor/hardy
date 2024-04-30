use super::*;
use hardy_cbor as cbor;
use hardy_proto::application::*;
use tokio::sync::mpsc::*;

#[derive(Clone)]
struct Config {
    status_reports: bool,
    forwarding: bool,
}

impl Config {
    fn load(config: &config::Config) -> Result<Self, anyhow::Error> {
        Ok(Self {
            status_reports: settings::get_with_default(config, "status_reports", false)?,
            forwarding: settings::get_with_default(config, "forwarding", true)?,
        })
    }
}

pub struct Dispatcher {
    node_id: node_id::NodeId,
    store: store::Store,
    tx: Sender<(Option<ingress::ClaSource>, bundle::Metadata, bundle::Bundle)>,
    config: Config,
    app_registry: app_registry::AppRegistry,
}

impl Clone for Dispatcher {
    fn clone(&self) -> Self {
        Self {
            node_id: self.node_id.clone(),
            store: self.store.clone(),
            tx: self.tx.clone(),
            config: self.config.clone(),
            app_registry: self.app_registry.clone(),
        }
    }
}

impl Dispatcher {
    pub fn new(
        config: &config::Config,
        node_id: node_id::NodeId,
        store: store::Store,
        app_registry: app_registry::AppRegistry,
        task_set: &mut tokio::task::JoinSet<()>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<Self, anyhow::Error> {
        // Load config
        let config = Config::load(config)?;

        // Create a channel for bundles
        let (tx, rx) = channel(16);
        let dispatcher = Self {
            node_id,
            store,
            tx,
            config,
            app_registry,
        };

        // Spawn a bundle receiver
        let dispatcher_cloned = dispatcher.clone();
        task_set
            .spawn(async move { Self::pipeline_pump(dispatcher_cloned, rx, cancel_token).await });

        Ok(dispatcher)
    }

    async fn enqueue_bundle(
        &self,
        from: Option<ingress::ClaSource>,
        metadata: bundle::Metadata,
        bundle: bundle::Bundle,
    ) -> Result<(), anyhow::Error> {
        // Put bundle into channel
        self.tx
            .send((from, metadata, bundle))
            .await
            .map_err(|e| e.into())
    }

    async fn pipeline_pump(
        self,
        mut rx: Receiver<(Option<ingress::ClaSource>, bundle::Metadata, bundle::Bundle)>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        // We're going to spawn a bunch of tasks
        let mut task_set = tokio::task::JoinSet::new();

        loop {
            tokio::select! {
                bundle = rx.recv() => match bundle {
                    None => break,
                    Some((from,metadata,bundle)) => {
                        let dispatcher = self.clone();
                        task_set.spawn(async move {
                            dispatcher.process_bundle(from,metadata,bundle).await.log_expect("Failed to process bundle");
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

    pub async fn process_bundle(
        &self,
        from: Option<ingress::ClaSource>,
        mut metadata: bundle::Metadata,
        mut bundle: bundle::Bundle,
    ) -> Result<(), anyhow::Error> {
        if let Some(from) = &from {
            if let Some(previous_node) = &bundle.previous_node {
                // Record a route to 'previous_node' via 'from'
                self.add_cla_route(previous_node, from)?;
            } else {
                // Record a route to bundle source via 'from'
                self.add_cla_route(&bundle.id.source, from)?
            }
        }

        if let bundle::BundleStatus::DispatchPending = &metadata.status {
            // Check if we are the final destination
            let new_status = if self.node_id.is_local_service(&bundle.destination) {
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
            return self.forward_bundle(from, metadata, bundle).await;
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
            if let Some(endpoint) = self.app_registry.lookup_by_eid(&bundle.destination) {
                // Notify that the bundle is ready
                endpoint.collection_notify(&bundle.id).await;
            }
        }
        Ok(())
    }

    async fn forward_bundle(
        &self,
        _from: Option<ingress::ClaSource>,
        metadata: bundle::Metadata,
        bundle: bundle::Bundle,
    ) -> Result<(), anyhow::Error> {
        if !self.config.forwarding {
            /* If forwarding is disabled in the configuration, then we can only deliver bundles.
             * As we have decided that the bundle is not for a local service, we cannot deliver.
             * Therefore, we respond with a Destination endpoint ID unavailable report
             * and tombstone the bundle to ignore duplicates */
            self.report_bundle_deletion(
                &metadata,
                &bundle,
                bundle::StatusReportReasonCode::DestinationEndpointIDUnavailable,
            )
            .await?;
            return self.store.remove(&metadata.storage_name).await;
        }

        todo!()
    }

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
            .source(&self.node_id.get_admin_endpoint(&bundle.report_to))
            .destination(&bundle.report_to)
            .add_payload_block(new_bundle_status_report(
                metadata, bundle, reason, None, None, None,
            ))
            .build(&self.store)
            .await?;

        // And queue it up
        self.enqueue_bundle(None, metadata, bundle).await
    }

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
            .source(&self.node_id.get_admin_endpoint(&bundle.report_to))
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
        self.enqueue_bundle(None, metadata, bundle).await
    }

    fn add_cla_route(
        &self,
        to: &bundle::Eid,
        _from: &ingress::ClaSource,
    ) -> Result<(), anyhow::Error> {
        match to {
            bundle::Eid::Null => {
                /* ignore */
                Ok(())
            }
            bundle::Eid::LocalNode { service_number: _ } => {
                /* ignore */
                Ok(())
            }
            bundle::Eid::Ipn2 {
                allocator_id: _,
                node_number: _,
                service_number: _,
            }
            | bundle::Eid::Ipn3 {
                allocator_id: _,
                node_number: _,
                service_number: _,
            } => todo!(),
            bundle::Eid::Dtn {
                node_name: _,
                demux: _,
            } => todo!(),
        }
    }

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
            b = b.report_to(&self.node_id.get_admin_endpoint(&destination));
        }

        // Lifetime
        if let Some(lifetime) = request.lifetime {
            b = b.lifetime(lifetime);
        }

        // Add payload and build
        let (metadata, bundle) = b.add_payload_block(request.data).build(&self.store).await?;

        // And queue it up
        self.enqueue_bundle(None, metadata, bundle).await
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
