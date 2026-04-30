use super::*;

impl Dispatcher {
    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    pub(crate) async fn restart_bundle(
        self: &Arc<Self>,
        storage_name: Arc<str>,
        file_time: time::OffsetDateTime,
    ) {
        let Some(data) = self.store.load_data(&storage_name).await else {
            // Data has gone while we were restarting - the reaper hasn't started, so this is data loss.
            // This is safe as the metadata restart will report it if it's in the metadata store
            return;
        };

        // Parse the bundle (again, just in case we have changed policies etc)
        match hardy_bpv7::bundle::RewrittenBundle::parse(&data, self.key_provider()) {
            Ok(hardy_bpv7::bundle::RewrittenBundle::Valid {
                bundle,
                report_unsupported,
            }) => {
                // Check if the metadata_storage knows about this bundle
                if let Some(metadata) = self.store.confirm_exists(&bundle.id).await {
                    if metadata.storage_name.as_ref() != Some(&storage_name) {
                        if metadata.storage_name.is_none() {
                            warn!("Duplicate processed bundle data found: {storage_name}");
                        } else {
                            warn!(
                                "Duplicate valid bundle data found: {storage_name} != {:?}",
                                metadata.storage_name.as_ref()
                            );
                        }

                        // Remove spurious duplicate
                        self.store.delete_data(&storage_name).await;
                        metrics::counter!("bpa.restart.duplicate").increment(1);
                    } else {
                        // Resume processing based on checkpoint status
                        match &metadata.status {
                            bundle::BundleStatus::New => {
                                // Ingress filter not yet complete - run full ingestion
                                let bundle = bundle::Bundle { metadata, bundle };
                                self.ingest_bundle(bundle, data).await;
                            }
                            bundle::BundleStatus::Dispatching => {
                                // Ingress filter done - enqueue for routing
                                let bundle = bundle::Bundle { metadata, bundle };
                                metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&bundle.metadata.status)).increment(1.0);
                                self.dispatch_bundle(bundle).await;
                            }
                            bundle::BundleStatus::ForwardPending { .. } => {
                                // Peer ID is stale after restart — reset to Waiting
                                let mut bundle = bundle::Bundle { metadata, bundle };
                                metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&bundle.metadata.status)).increment(1.0);
                                self.store
                                    .update_status(&mut bundle, &bundle::BundleStatus::Waiting)
                                    .await;
                            }
                            // Other statuses are handled by their respective recovery mechanisms:
                            // - Waiting: poll_waiting recovery
                            // - WaitingForService: poll_service_waiting on service re-registration
                            // - AduFragment: fragment reassembly polling
                            _ => {
                                metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&metadata.status)).increment(1.0);
                            }
                        }
                    }
                } else {
                    // Effectively a new bundle
                    let bundle = bundle::Bundle {
                        metadata: bundle::BundleMetadata {
                            storage_name: Some(storage_name),
                            read_only: bundle::ReadOnlyMetadata {
                                received_at: file_time,
                                ..Default::default()
                            },
                            ..Default::default()
                        },
                        bundle,
                    };

                    // Save the metadata
                    self.store.insert_metadata(&bundle).await;

                    // Report we have received the bundle
                    self.report_bundle_reception(
                        &bundle,
                        if report_unsupported {
                            hardy_bpv7::status_report::ReasonCode::BlockUnsupported
                        } else {
                            hardy_bpv7::status_report::ReasonCode::NoAdditionalInformation
                        },
                    )
                    .await;

                    // Dispatch the 'new' bundle via processing pool
                    self.ingest_bundle(bundle, data).await;

                    // Report the bundle as an orphan
                    metrics::counter!("bpa.restart.orphan").increment(1);
                }
            }
            Ok(hardy_bpv7::bundle::RewrittenBundle::Rewritten {
                bundle,
                new_data,
                report_unsupported,
                non_canonical: _,
            }) => {
                warn!("Bundle in non-canonical format found: {storage_name}");

                // Remove the previous from bundle_storage
                self.store.delete_data(&storage_name).await;

                // Check if the metadata_storage knows about this bundle
                let exists = if let Some(metadata) = self.store.confirm_exists(&bundle.id).await {
                    if metadata.storage_name.as_ref() != Some(&storage_name) {
                        if metadata.storage_name.is_none() {
                            warn!(
                                "Non-canonical copy of processed bundle data found: {storage_name}"
                            );
                        } else {
                            warn!(
                                "Duplicate non-canonical bundle data found: {storage_name} != {:?}",
                                metadata.storage_name.as_ref()
                            );
                        }

                        metrics::counter!("bpa.restart.duplicate").increment(1);
                        return;
                    }
                    true
                } else {
                    false
                };

                // Write the rewritten bundle now for safety
                let data = Bytes::from(new_data);
                let new_storage_name = self.store.save_data(data.clone()).await;

                let bundle = bundle::Bundle {
                    metadata: bundle::BundleMetadata {
                        storage_name: Some(new_storage_name),
                        read_only: bundle::ReadOnlyMetadata {
                            received_at: file_time,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    bundle,
                };

                // Whatever we have in the metadata store is non-canonical

                if !exists {
                    // Save the metadata
                    self.store.insert_metadata(&bundle).await;

                    // Report we have received the bundle
                    self.report_bundle_reception(
                        &bundle,
                        if report_unsupported {
                            hardy_bpv7::status_report::ReasonCode::BlockUnsupported
                        } else {
                            hardy_bpv7::status_report::ReasonCode::NoAdditionalInformation
                        },
                    )
                    .await;
                } else {
                    // Replace the metadata
                    self.store.update_metadata(&bundle).await;
                }

                // Dispatch the 'new' bundle via processing pool
                self.ingest_bundle(bundle, data).await;

                // Report the bundle as an orphan
                metrics::counter!("bpa.restart.orphan").increment(1);
            }
            Ok(hardy_bpv7::bundle::RewrittenBundle::Invalid {
                bundle,
                reason,
                error,
            }) => {
                warn!("Invalid bundle found: {storage_name}, {error}");

                // Remove it from bundle_storage, it shouldn't be there
                self.store.delete_data(&storage_name).await;
                metrics::counter!("bpa.restart.junk").increment(1);

                // Check if the metadata_storage knows about this bundle
                if let Some(metadata) = self.store.confirm_exists(&bundle.id).await {
                    if metadata.storage_name.as_ref() != Some(&storage_name) {
                        if metadata.storage_name.is_none() {
                            warn!("Invalid copy of processed bundle data found: {storage_name}");
                        } else {
                            warn!(
                                "Duplicate invalid bundle data found: {storage_name} != {:?}",
                                metadata.storage_name.as_ref()
                            );
                        }
                    } else {
                        // Previously accepted bundle — send deletion report and tombstone
                        let bundle = bundle::Bundle {
                            metadata: bundle::BundleMetadata {
                                read_only: bundle::ReadOnlyMetadata {
                                    received_at: file_time,
                                    ..Default::default()
                                },
                                ..Default::default()
                            },
                            bundle,
                        };

                        metrics::counter!("bpa.bundle.dropped", "reason" => crate::otel_metrics::reason_label(&reason)).increment(1);
                        self.report_bundle_deletion(&bundle, reason).await;
                        self.store.tombstone_metadata(&bundle.bundle.id).await;
                    }
                }
            }
            Err(e) => {
                // Parse failed badly, no idea who to report to
                warn!("Junk data found: {storage_name}, {e}");

                // TODO:  This is where we can wrap the damaged bundle in a "Junk Bundle Payload" and forward it to a 'lost+found' endpoint.  For now we just drop it.

                // Drop the bundle
                self.store.delete_data(&storage_name).await;
                metrics::counter!("bpa.restart.junk").increment(1);
            }
        }
    }
}
