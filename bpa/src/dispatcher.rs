use super::*;
use hardy_cbor as cbor;
use tokio::sync::mpsc::*;

pub struct Dispatcher {
    store: store::Store,
    status_reports: bool,
    tx: Sender<(bundle::Metadata, bundle::Bundle)>,
    source_eid: bundle::Eid,
}

impl Clone for Dispatcher {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            status_reports: self.status_reports,
            tx: self.tx.clone(),
            source_eid: self.source_eid.clone(),
        }
    }
}

impl Dispatcher {
    pub fn new(
        config: &config::Config,
        store: store::Store,
        task_set: &mut tokio::task::JoinSet<()>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<Self, anyhow::Error> {
        // Load NodeId from config
        let source_eid = match config.get::<String>("node_id") {
            Ok(source_eid) => match source_eid.parse() {
                Ok(source_eid) => source_eid,
                Err(e) => return Err(anyhow!("Invalid \"node_id\" in configuration: {}", e)),
            },
            Err(e) => return Err(anyhow!("Missing \"node_id\" from configuration: {}", e)),
        };

        // Confirm we have a valid EID with administrative endpoint service number
        let source_eid = match source_eid {
            bundle::Eid::Ipn3 {
                allocator_id: _,
                node_number: _,
                service_number: 0,
            } => source_eid,
            bundle::Eid::Dtn {
                node_name: _,
                demux: _,
            } => source_eid,
            e => return Err(anyhow!("Invalid \"node_id\" in configuration: {}", e)),
        };
        log::info!("Local Node ID: {source_eid}");

        // Create a channel for bundles
        let (tx, rx) = channel(16);
        let dispatcher = Self {
            store,
            status_reports: settings::get_with_default(config, "status_reports", false)?,
            tx,
            source_eid,
        };

        // Spawn a bundle receiver
        let dispatcher_cloned = dispatcher.clone();
        task_set
            .spawn(async move { Self::pipeline_pump(dispatcher_cloned, rx, cancel_token).await });

        Ok(dispatcher)
    }

    pub async fn enqueue_bundle(
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
                _ = cancel_token.cancelled() => break
            }
        }

        // Wait for all sub-tasks to complete
        while let Some(r) = task_set.join_next().await {
            r.log_expect("Task terminated unexpectedly")
        }
    }

    async fn process_bundle(
        &self,
        metadata: bundle::Metadata,
        bundle: bundle::Bundle,
    ) -> Result<(), anyhow::Error> {
        // This is the meat of the dispatch pipeline
        todo!()
    }

    pub async fn report_bundle_reception(
        &self,
        metadata: &bundle::Metadata,
        bundle: &bundle::Bundle,
        reason: bundle::StatusReportReasonCode,
    ) -> Result<(), anyhow::Error> {
        // Check if a report is requested
        if !self.status_reports || !bundle.flags.receipt_report_requested {
            return Ok(());
        }

        // Create a bundle report
        let (metadata, bundle) = bundle::Builder::new(bundle::BundleStatus::DispatchPending)
            .is_admin_record(true)
            .source(self.source_eid.clone())
            .destination(bundle.report_to.clone())
            .add_payload_block(new_bundle_status_report(
                metadata, bundle, reason, None, None, None,
            ))
            .build(&self.store)
            .await?;

        // And queue it up
        self.enqueue_bundle(metadata, bundle).await
    }

    pub async fn report_bundle_deletion(
        &self,
        metadata: &bundle::Metadata,
        bundle: &bundle::Bundle,
        reason: bundle::StatusReportReasonCode,
    ) -> Result<(), anyhow::Error> {
        // Check if a report is requested
        if !self.status_reports || !bundle.flags.delete_report_requested {
            return Ok(());
        }

        // Create a bundle report
        let (metadata, bundle) = bundle::Builder::new(bundle::BundleStatus::DispatchPending)
            .is_admin_record(true)
            .source(self.source_eid.clone())
            .destination(bundle.report_to.clone())
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
