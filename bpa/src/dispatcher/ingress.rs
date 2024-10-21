use super::*;

impl Dispatcher {
    #[instrument(skip(self, data))]
    pub async fn receive_bundle(&self, data: Bytes) -> Result<(), Error> {
        // Capture received_at as soon as possible
        let received_at = Some(time::OffsetDateTime::now_utc());

        // Do a fast pre-check
        if data.is_empty() {
            return Err(cbor::decode::Error::NotEnoughData.into());
        } else if data[0] == 0x06 {
            trace!("Data looks like a BPv6 bundle");
            return Err(cbor::decode::Error::IncorrectType(
                "BPv7 bundle".to_string(),
                "Possible BPv6 bundle".to_string(),
            )
            .into());
        }

        // Parse the bundle
        let (bundle, data) = match cbor::decode::parse(&data)? {
            (bpv7::ValidBundle::Valid(bundle), true) => (bundle, data.to_vec()),
            (bpv7::ValidBundle::Valid(mut bundle), false) => {
                // Rewrite the bundle
                let data = bundle.canonicalise(&data)?;
                (bundle, data)
            }
            (bpv7::ValidBundle::Invalid(bundle), _) => {
                // Receive a fake bundle
                return self
                    .receive_inner(
                        metadata::Bundle {
                            metadata: metadata::Metadata {
                                status: metadata::BundleStatus::Tombstone(
                                    time::OffsetDateTime::now_utc(),
                                ),
                                storage_name: None,
                                hash: None,
                                received_at,
                            },
                            bundle,
                        },
                        false,
                    )
                    .await;
            }
        };

        // Write the bundle data to the store
        let (storage_name, hash) = self.store.store_data(data.into()).await?;

        if let Err(e) = self
            .receive_inner(
                metadata::Bundle {
                    metadata: metadata::Metadata {
                        storage_name: Some(storage_name.clone()),
                        hash: Some(hash),
                        received_at,
                        ..Default::default()
                    },
                    bundle,
                },
                true,
            )
            .await
        {
            // If we failed to process the bundle, remove the data
            self.store.delete_data(&storage_name).await?;
            Err(e)
        } else {
            Ok(())
        }
    }

    #[instrument(skip(self))]
    async fn receive_inner(&self, bundle: metadata::Bundle, valid: bool) -> Result<(), Error> {
        // Report we have received the bundle
        self.report_bundle_reception(
            &bundle,
            bpv7::StatusReportReasonCode::NoAdditionalInformation,
        )
        .await?;

        /* RACE: If there is a crash between the report creation(above) and the metadata store (below)
         *  then we may send more than one "Received" Status Report when restarting,
         *  but that is currently considered benign (as a duplicate report causes little harm)
         *  and unlikely (as the report forwarding process is expected to take longer than the metadata.store)
         */

        if !self
            .store
            .store_metadata(&bundle.metadata, &bundle.bundle)
            .await?
        {
            // Bundle with matching id already exists in the metadata store
            trace!("Bundle with matching id already exists in the metadata store");

            // Drop the stored data if it was valid, and do not process further
            if let Some(storage_name) = bundle.metadata.storage_name {
                self.store.delete_data(&storage_name).await?;
            }
            Ok(())
        } else {
            // Check the bundle further
            self.check_bundle(bundle, valid).await
        }
    }

    #[instrument(skip(self))]
    pub async fn restart_bundle(
        &self,
        mut bundle: metadata::Bundle,
        valid: bool,
        orphan: bool,
    ) -> Result<(), Error> {
        if orphan {
            // If the bundle isn't valid, it will always be a Tombstone
            if !valid {
                bundle.metadata.status =
                    metadata::BundleStatus::Tombstone(time::OffsetDateTime::now_utc())
            }

            // Report we have received the bundle
            self.report_bundle_reception(
                &bundle,
                bpv7::StatusReportReasonCode::NoAdditionalInformation,
            )
            .await?;

            /* RACE: If there is a crash between the report creation(above) and the metadata store (below)
             *  then we may send more than one "Received" Status Report when restarting,
             *  but that is currently considered benign (as a duplicate report causes little harm)
             *  and unlikely (as the report forwarding process is expected to take longer than the metadata.store)
             */

            if !self
                .store
                .store_metadata(&bundle.metadata, &bundle.bundle)
                .await?
            {
                /* Bundle with matching id already exists in the metadata store
                 * This can happen if we are receiving new bundles as we spool through restarted bundles
                 */
                trace!("Bundle with matching id already exists in the metadata store");

                // Drop the stored data, and do not process further
                return self
                    .store
                    .delete_data(&bundle.metadata.storage_name.unwrap())
                    .await;
            }
        }

        self.check_bundle(bundle, valid).await
    }

    #[instrument(skip(self))]
    async fn check_bundle(&self, bundle: metadata::Bundle, valid: bool) -> Result<(), Error> {
        if !valid {
            // Not valid, drop it
            return self
                .drop_bundle(
                    bundle,
                    Some(bpv7::StatusReportReasonCode::BlockUnintelligible),
                )
                .await;
        }

        /* Always check bundles, no matter the state, as after restarting
         * the configured filters or code may have changed, and reprocessing is desired.
         */

        if bundle.bundle.flags.unrecognised != 0 {
            trace!(
                "Bundle primary block has unrecognised flag bits set: {:#x}",
                bundle.bundle.flags.unrecognised
            );
        }

        // Check some basic semantic validity, lifetime first
        let mut reason = bundle
            .has_expired()
            .then(|| {
                trace!("Bundle lifetime has expired");
                bpv7::StatusReportReasonCode::LifetimeExpired
            })
            .or_else(|| {
                // Check hop count exceeded
                bundle.bundle.hop_count.and_then(|hop_info| {
                    (hop_info.count >= hop_info.limit).then(|| {
                        trace!(
                            "Bundle hop-limit {}/{} exceeded",
                            hop_info.count,
                            hop_info.limit
                        );
                        bpv7::StatusReportReasonCode::HopLimitExceeded
                    })
                })
            });

        if reason.is_none() {
            // Check extension blocks
            reason = self.check_extension_blocks(&bundle).await?;
        }

        if reason.is_none() {
            // TODO: BPSec here!
        }

        if reason.is_none() {
            // TODO: Pluggable Ingress filters!
        }

        if let Some(reason) = reason {
            // Not valid, drop it
            return self.drop_bundle(bundle, Some(reason)).await;
        }

        // Now process in parallel
        self.dispatch_bundle(bundle).await
    }

    async fn check_extension_blocks(
        &self,
        bundle: &metadata::Bundle,
    ) -> Result<Option<bpv7::StatusReportReasonCode>, Error> {
        let mut unsupported = false;
        for (block_number, block) in &bundle.bundle.blocks {
            if block.flags.unrecognised != 0 {
                trace!(
                    "Block {block_number} has unrecognised flag bits set: {:#x}",
                    block.flags.unrecognised
                );
            }

            if let bpv7::BlockType::Unrecognised(value) = &block.block_type {
                if value <= &191 {
                    trace!("Extension block {block_number} uses unrecognised type code {value}");
                }

                if block.flags.report_on_failure {
                    // Only report once!
                    if !unsupported {
                        self.report_bundle_reception(
                            bundle,
                            bpv7::StatusReportReasonCode::BlockUnsupported,
                        )
                        .await?;
                        unsupported = true;
                    }
                }

                if block.flags.delete_bundle_on_failure {
                    return Ok(Some(bpv7::StatusReportReasonCode::BlockUnsupported));
                }
            }
        }
        Ok(None)
    }
}
