use super::*;
use core::ops::Deref;
use hardy_bpv7::status_report::ReasonCode;

impl Dispatcher {
    #[instrument(skip(self, data))]
    pub async fn receive_bundle(self: &Arc<Self>, data: Bytes) -> cla::Result<()> {
        // Capture received_at as soon as possible
        let received_at = Some(time::OffsetDateTime::now_utc());

        // Do a fast pre-check
        match data.first() {
            None => {
                return Err(hardy_bpv7::Error::InvalidCBOR(
                    hardy_cbor::decode::Error::NotEnoughData,
                )
                .into());
            }
            Some(0x06) => {
                trace!("Data looks like a BPv6 bundle");
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
            match hardy_bpv7::bundle::ValidBundle::parse(&data, self.deref())? {
                hardy_bpv7::bundle::ValidBundle::Valid(bundle, report_unsupported) => (
                    bundle::Bundle {
                        metadata: BundleMetadata {
                            storage_name: Some(self.store.save_data(data).await?),
                            received_at,
                        },
                        bundle,
                    },
                    None,
                    report_unsupported,
                ),
                hardy_bpv7::bundle::ValidBundle::Rewritten(bundle, data, report_unsupported) => {
                    trace!("Received bundle has been rewritten");
                    (
                        bundle::Bundle {
                            metadata: BundleMetadata {
                                storage_name: Some(self.store.save_data(data.into()).await?),
                                received_at,
                            },
                            bundle,
                        },
                        None,
                        report_unsupported,
                    )
                }
                hardy_bpv7::bundle::ValidBundle::Invalid(bundle, reason, e) => {
                    trace!("Invalid bundle received: {e}");

                    // Don't bother saving the bundle data, it's garbage
                    (
                        bundle::Bundle {
                            metadata: BundleMetadata {
                                storage_name: None,
                                received_at,
                            },
                            bundle,
                        },
                        Some(reason),
                        false,
                    )
                }
            };

        match self.store.insert_metadata(&bundle).await {
            Ok(false) => {
                // Bundle with matching id already exists in the metadata store

                // Drop the stored data and do not process further
                if let Some(storage_name) = &bundle.metadata.storage_name {
                    self.store.delete_data(storage_name).await?;
                }
                return Ok(());
            }
            Err(e) => {
                if let Some(storage_name) = &bundle.metadata.storage_name {
                    _ = self.store.delete_data(storage_name).await;
                }
                return Err(e.into());
            }
            _ => {}
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

        // Check the bundle further
        self.ingress_bundle(bundle, reason)
            .await
            .map_err(Into::into)
    }

    #[instrument(skip(self))]
    pub async fn ingress_bundle(
        self: &Arc<Self>,
        bundle: bundle::Bundle,
        mut reason: Option<ReasonCode>,
    ) -> Result<(), Error> {
        /* Always check bundles, no matter the state, as after restarting
         * the configured filters or code may have changed, and reprocessing is desired.
         */

        if let Some(u) = bundle.bundle.flags.unrecognised {
            trace!("Bundle primary block has unrecognised flag bits set: {u:#x}");
        }

        if reason.is_none() {
            // Check some basic semantic validity, lifetime first
            if bundle.has_expired() {
                trace!("Bundle lifetime has expired");
                reason = Some(ReasonCode::LifetimeExpired);
            } else if let Some(hop_info) = bundle.bundle.hop_count.as_ref() {
                // Check hop count exceeded
                if hop_info.count >= hop_info.limit {
                    trace!("Bundle hop-limit {} exceeded", hop_info.limit);
                    reason = Some(ReasonCode::HopLimitExceeded);
                }
            }
        }

        if reason.is_some() {
            // Not valid, drop it
            return self.drop_bundle(bundle, reason).await;
        }

        // Now process the bundle
        self.dispatch_bundle(bundle).await
    }

    #[instrument(skip(self))]
    pub async fn restart_bundle(
        self: &Arc<Self>,
        storage_name: Arc<str>,
        file_time: Option<time::OffsetDateTime>,
    ) -> Result<(u64, u64), Error> {
        let Some(data) = self.store.load_data(&storage_name).await? else {
            // Data has gone while we were restarting
            return Ok((0, 0));
        };

        // Parse the bundle (again, just in case we have changed policies etc)
        let (o, b, bundle, reason) =
            match hardy_bpv7::bundle::ValidBundle::parse(&data, self.deref()) {
                Ok(hardy_bpv7::bundle::ValidBundle::Valid(bundle, report_unsupported)) => {
                    // Check if the metadata_storage knows about this bundle
                    if let Some(metadata) = self.store.confirm_exists(&bundle.id).await? {
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
                            return self.store.delete_data(&storage_name).await.map(|_| (0, 1));
                        }
                        // All looks good, just continue dispatching
                        (0, 0, bundle::Bundle { bundle, metadata }, None)
                    } else {
                        let bundle = bundle::Bundle {
                            metadata: BundleMetadata {
                                storage_name: Some(storage_name),
                                received_at: file_time,
                            },
                            bundle,
                        };

                        // Save the metadata
                        self.store.insert_metadata(&bundle).await?;

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

                        // Effectively a new bundle
                        (1, 0, bundle, None)
                    }
                }
                Ok(hardy_bpv7::bundle::ValidBundle::Rewritten(
                    bundle,
                    data,
                    report_unsupported,
                )) => {
                    warn!("Bundle in non-canonical format found: {storage_name}");

                    // Check if the metadata_storage knows about this bundle
                    let exists =
                        if let Some(metadata) = self.store.confirm_exists(&bundle.id).await? {
                            if metadata.storage_name.as_ref() != Some(&storage_name) {
                                warn!("Duplicate non-canonical bundle data found: {storage_name}");

                                // Remove spurious duplicate
                                return self.store.delete_data(&storage_name).await.map(|_| (0, 1));
                            }
                            true
                        } else {
                            false
                        };

                    // Write the rewritten bundle now for safety
                    let new_storage_name = self.store.save_data(data.into()).await?;

                    // Remove the previous from bundle_storage
                    self.store.delete_data(&storage_name).await?;

                    let bundle = bundle::Bundle {
                        metadata: BundleMetadata {
                            storage_name: Some(new_storage_name),
                            received_at: file_time,
                        },
                        bundle,
                    };

                    // Whatever we have in the metadata store is non-canonical

                    if !exists {
                        // Save the metadata
                        self.store.insert_metadata(&bundle).await?;

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
                        self.store.update_metadata(&bundle).await?;
                    }

                    // Report the bundle as an orphan
                    (1, 0, bundle, None)
                }
                Ok(hardy_bpv7::bundle::ValidBundle::Invalid(bundle, reason, e)) => {
                    warn!("Invalid bundle found: {storage_name}, {e}");

                    // Check if the metadata_storage knows about this bundle
                    let exists =
                        if let Some(metadata) = self.store.confirm_exists(&bundle.id).await? {
                            if metadata.storage_name.as_ref() != Some(&storage_name) {
                                warn!("Duplicate invalid bundle data found: {storage_name}");

                                // Remove spurious duplicate
                                return self.store.delete_data(&storage_name).await.map(|_| (0, 1));
                            }
                            true
                        } else {
                            false
                        };

                    // Remove it from bundle_storage, it shouldn't be there
                    self.store.delete_data(&storage_name).await?;

                    // Whatever we have in the store isn't correct

                    let bundle = bundle::Bundle {
                        metadata: BundleMetadata {
                            storage_name: None,
                            received_at: file_time,
                        },
                        bundle,
                    };

                    if !exists {
                        // Save the metadata
                        self.store.insert_metadata(&bundle).await?;

                        // Report we have received the bundle
                        self.report_bundle_reception(
                            &bundle,
                            hardy_bpv7::status_report::ReasonCode::NoAdditionalInformation,
                        )
                        .await;
                    } else {
                        // Replace the metadata
                        self.store.update_metadata(&bundle).await?;
                    }

                    (0, 1, bundle, Some(reason))
                }
                Err(e) => {
                    // Parse failed badly, no idea who to report to
                    warn!("Junk data found: {storage_name}, {e}");

                    // Drop the bundle
                    return self.store.delete_data(&storage_name).await.map(|_| (0, 1));
                }
            };

        // Process the 'new' bundle
        self.ingress_bundle(bundle, reason).await.map(|_| (o, b))
    }
}
