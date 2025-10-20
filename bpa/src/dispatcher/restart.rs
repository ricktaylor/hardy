use super::*;
use storage::recover::RestartResult;

impl Dispatcher {
    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    pub(crate) async fn restart_bundle(
        self: &Arc<Self>,
        storage_name: Arc<str>,
        file_time: time::OffsetDateTime,
    ) -> RestartResult {
        let Some(data) = self.store.load_data(&storage_name).await else {
            // Data has gone while we were restarting
            return RestartResult::Missing;
        };

        // Parse the bundle (again, just in case we have changed policies etc)
        match hardy_bpv7::bundle::ValidBundle::parse(&data, self.key_store()) {
            Ok(hardy_bpv7::bundle::ValidBundle::Valid(bundle, report_unsupported)) => {
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
                        RestartResult::Duplicate
                    } else {
                        // All good, no further action required
                        RestartResult::Valid
                    }
                } else {
                    // Effectively a new bundle
                    let bundle = bundle::Bundle {
                        metadata: BundleMetadata {
                            storage_name: Some(storage_name),
                            received_at: file_time,
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

                    self.process_bundle(bundle, None).await;
                    RestartResult::Orphan
                }
            }
            Ok(hardy_bpv7::bundle::ValidBundle::Rewritten(
                bundle,
                data,
                report_unsupported,
                non_canonical,
            )) => {
                warn!("Bundle in non-canonical format found: {storage_name}");

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

                        // Remove spurious duplicate
                        self.store.delete_data(&storage_name).await;
                        return RestartResult::Duplicate;
                    }
                    true
                } else {
                    false
                };

                // Write the rewritten bundle now for safety
                let new_storage_name = self.store.save_data(data.into()).await;

                // Remove the previous from bundle_storage
                self.store.delete_data(&storage_name).await;

                let bundle = bundle::Bundle {
                    metadata: BundleMetadata {
                        storage_name: Some(new_storage_name),
                        received_at: file_time,
                        non_canonical,
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

                // Report the bundle as an orphan
                self.process_bundle(bundle, None).await;
                RestartResult::Orphan
            }
            Ok(hardy_bpv7::bundle::ValidBundle::Invalid(bundle, reason, e)) => {
                warn!("Invalid bundle found: {storage_name}, {e}");

                // Check if the metadata_storage knows about this bundle
                let exists = if let Some(metadata) = self.store.confirm_exists(&bundle.id).await {
                    if metadata.storage_name.as_ref() != Some(&storage_name) {
                        if metadata.storage_name.is_none() {
                            warn!("Invalid copy of processed bundle data found: {storage_name}");
                        } else {
                            warn!(
                                "Duplicate invalid bundle data found: {storage_name} != {:?}",
                                metadata.storage_name.as_ref()
                            );
                        }

                        // Remove spurious duplicate
                        self.store.delete_data(&storage_name).await;
                        return RestartResult::Duplicate;
                    }
                    true
                } else {
                    false
                };

                // Remove it from bundle_storage, it shouldn't be there
                self.store.delete_data(&storage_name).await;

                // Whatever we have in the store isn't correct

                let bundle = bundle::Bundle {
                    metadata: BundleMetadata {
                        received_at: file_time,
                        ..Default::default()
                    },
                    bundle,
                };

                if !exists {
                    // Save the metadata
                    self.store.insert_metadata(&bundle).await;

                    // Report we have received the bundle
                    self.report_bundle_reception(
                        &bundle,
                        hardy_bpv7::status_report::ReasonCode::NoAdditionalInformation,
                    )
                    .await;
                } else {
                    // Replace the metadata
                    self.store.update_metadata(&bundle).await;
                }

                // Process the 'new' bundle
                self.process_bundle(bundle, Some(reason)).await;
                RestartResult::Orphan
            }
            Err(e) => {
                // Parse failed badly, no idea who to report to
                warn!("Junk data found: {storage_name}, {e}");

                // Drop the bundle
                self.store.delete_data(&storage_name).await;
                RestartResult::Junk
            }
        }
    }
}
