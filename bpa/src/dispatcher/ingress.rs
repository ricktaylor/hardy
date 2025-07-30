use std::ops::Deref;

use super::*;
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
                hardy_bpv7::bundle::ValidBundle::Valid(bundle, report_unsupported) => {
                    // Write the bundle data to the store
                    let hash = store::hash(&data);
                    let Some(storage_name) = self.store.store_data(data, hash.clone()).await?
                    else {
                        // Duplicate
                        return Ok(());
                    };
                    (
                        bundle::Bundle {
                            metadata: BundleMetadata {
                                storage_name: Some(storage_name),
                                hash: Some(hash),
                                received_at,
                            },
                            bundle,
                        },
                        None,
                        report_unsupported,
                    )
                }
                hardy_bpv7::bundle::ValidBundle::Rewritten(bundle, data, report_unsupported) => {
                    trace!("Received bundle has been rewritten");

                    // Write the bundle data to the store
                    let hash = store::hash(&data);
                    let Some(storage_name) =
                        self.store.store_data(data.into(), hash.clone()).await?
                    else {
                        // Duplicate
                        return Ok(());
                    };
                    (
                        bundle::Bundle {
                            metadata: BundleMetadata {
                                storage_name: Some(storage_name),
                                hash: Some(hash),
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
                                hash: None,
                                received_at,
                            },
                            bundle,
                        },
                        Some(reason),
                        false,
                    )
                }
            };

        match self.store.store_metadata(&bundle).await {
            Ok(false) => {
                // Bundle with matching id already exists in the metadata store
                //trace!("Bundle with matching id already exists in the metadata store");

                // Drop the stored data if it was valid, and do not process further
                return self
                    .store
                    .remove_data(&bundle.metadata)
                    .await
                    .map_err(Into::into);
            }
            Err(e) => {
                return self
                    .store
                    .remove_data(&bundle.metadata)
                    .await
                    .and(Err(e))
                    .map_err(Into::into);
            }
            _ => {}
        };

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
    ) -> (u64, u64) {
        let Some(data) = self
            .store
            .load_data(&storage_name)
            .await
            .trace_expect(&format!("Failed to load bundle data: {storage_name}"))
        else {
            // Data has gone while we were restarting
            return (0, 0);
        };

        let hash = store::hash(&data);

        // Parse the bundle (again, just in case we have changed policies etc)
        let (o, b, bundle, reason) =
            match hardy_bpv7::bundle::ValidBundle::parse(&data, self.deref()) {
                Ok(hardy_bpv7::bundle::ValidBundle::Valid(bundle, report_unsupported)) => {
                    // Check if the metadata_storage knows about this bundle
                    if let Some(metadata) = self
                        .store
                        .confirm_exists(&bundle.id)
                        .await
                        .trace_expect("Failed to confirm bundle existence")
                    {
                        if metadata.storage_name.as_ref() != Some(&storage_name)
                            || metadata.hash.as_ref() != Some(&hash)
                        {
                            warn!("Duplicate bundle data found: {storage_name}");

                            // Remove spurious duplicate
                            self.store
                                .remove_data(&BundleMetadata {
                                    storage_name: Some(storage_name.clone()),
                                    hash: Some(hash),
                                    received_at: file_time,
                                })
                                .await
                                .trace_expect(&format!(
                                    "Failed to remove duplicate bundle: {storage_name}"
                                ));
                            return (0, 1);
                        }
                        // All looks good, just continue validation
                        (0, 0, bundle::Bundle { bundle, metadata }, None)
                    } else {
                        let bundle = bundle::Bundle {
                            metadata: BundleMetadata {
                                storage_name: Some(storage_name),
                                hash: Some(hash),
                                received_at: file_time,
                            },
                            bundle,
                        };

                        // Save the metadata
                        self.store
                            .store_metadata(&bundle)
                            .await
                            .trace_expect("Failed to store bundle");

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

                    // Write the rewritten bundle now for safety
                    let new_hash = store::hash(&data);
                    let new_storage_name = self
                        .store
                        .store_data(data.into(), new_hash.clone())
                        .await
                        .trace_expect("Failed to store rewritten canonical bundle");

                    // Whatever we have in the store is non-canonical
                    // Confirm it exists
                    let exists = if self
                        .store
                        .confirm_exists(&bundle.id)
                        .await
                        .trace_expect("Failed to confirm bundle existence")
                        .is_some()
                    {
                        warn!("Non-canonical bundle data found: {storage_name}");

                        // And remove it
                        self.store.remove_metadata(&bundle.id).await.trace_expect(
                            "Failed to remove rewritten canonical bundle original metadata",
                        );
                        true
                    } else {
                        false
                    };

                    // Remove the previous from bundle_storage
                    self.store
                        .remove_data(&BundleMetadata {
                            storage_name: Some(storage_name.clone()),
                            hash: Some(hash),
                            received_at: file_time,
                        })
                        .await
                        .trace_expect(&format!(
                            "Failed to remove duplicate bundle: {storage_name}"
                        ));

                    let Some(new_storage_name) = new_storage_name else {
                        // The rewritten bundle is already in the bundle store!!
                        return (0, 1);
                    };

                    let bundle = bundle::Bundle {
                        metadata: BundleMetadata {
                            storage_name: Some(new_storage_name),
                            hash: Some(new_hash),
                            received_at: file_time,
                        },
                        bundle,
                    };

                    // Re-save the metadata
                    self.store
                        .store_metadata(&bundle)
                        .await
                        .trace_expect("Failed to store bundle");

                    if !exists {
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
                    }

                    // Treat the bundle as an orphan
                    (1, 0, bundle, None)
                }
                Ok(hardy_bpv7::bundle::ValidBundle::Invalid(bundle, reason, e)) => {
                    warn!("Invalid bundle found: {storage_name}, {e}");

                    // Remove it from bundle_storage, it shouldn't be there
                    self.store
                        .remove_data(&BundleMetadata {
                            storage_name: Some(storage_name.clone()),
                            hash: Some(hash),
                            received_at: file_time,
                        })
                        .await
                        .trace_expect(&format!(
                            "Failed to remove duplicate bundle: {storage_name}"
                        ));

                    // Whatever we have in the store isn't correct
                    // Confirm it exists
                    let exists = if self
                        .store
                        .confirm_exists(&bundle.id)
                        .await
                        .trace_expect("Failed to confirm bundle existence")
                        .is_some()
                    {
                        // And remove it
                        self.store.remove_metadata(&bundle.id).await.trace_expect(
                            "Failed to remove rewritten canonical bundle original metadata",
                        );
                        true
                    } else {
                        false
                    };

                    let bundle = bundle::Bundle {
                        metadata: BundleMetadata {
                            storage_name: None,
                            hash: None,
                            received_at: file_time,
                        },
                        bundle,
                    };

                    // Save the correct metadata
                    self.store
                        .store_metadata(&bundle)
                        .await
                        .trace_expect("Failed to store bundle");

                    if !exists {
                        // Report we have received the bundle
                        self.report_bundle_reception(
                            &bundle,
                            hardy_bpv7::status_report::ReasonCode::NoAdditionalInformation,
                        )
                        .await;
                    }

                    (0, 1, bundle, Some(reason))
                }
                Err(e) => {
                    // Parse failed badly, no idea who to report to
                    warn!("Junk data found: {storage_name}, {e}");

                    // Drop the bundle
                    self.store
                        .remove_data(&BundleMetadata {
                            storage_name: Some(storage_name.clone()),
                            hash: Some(hash),
                            received_at: file_time,
                        })
                        .await
                        .trace_expect(&format!(
                            "Failed to remove malformed bundle: {storage_name}"
                        ));

                    return (0, 1);
                }
            };

        // Check the bundle further
        self.ingress_bundle(bundle, reason)
            .await
            .trace_expect("Bundle validation failed!");

        (o, b)
    }
}
