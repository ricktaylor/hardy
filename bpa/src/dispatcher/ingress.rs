use super::*;

impl Dispatcher {
    #[instrument(skip(self, data))]
    pub async fn receive_bundle(&self, data: &[u8]) -> cla::Result<()> {
        // Capture received_at as soon as possible
        let received_at = Some(time::OffsetDateTime::now_utc());

        // Do a fast pre-check
        if data.is_empty() {
            return Err(bpv7::Error::InvalidCBOR(cbor::decode::Error::NotEnoughData).into());
        } else if data[0] == 0x06 {
            trace!("Data looks like a BPv6 bundle");
            return Err(bpv7::Error::InvalidCBOR(cbor::decode::Error::IncorrectType(
                "BPv7 bundle".to_string(),
                "Possible BPv6 bundle".to_string(),
            ))
            .into());
        }

        // Parse the bundle
        match bpv7::ValidBundle::parse(data, |_, _| Ok(None))? {
            bpv7::ValidBundle::Valid(bundle, report_unsupported) => {
                // Write the bundle data to the store
                let (storage_name, hash) = self.store.store_data(data).await?;
                self.ingress_bundle(
                    bundle::Bundle {
                        metadata: BundleMetadata {
                            storage_name: Some(storage_name),
                            hash: Some(hash),
                            received_at,
                            ..Default::default()
                        },
                        bundle,
                    },
                    None,
                    report_unsupported,
                )
            }
            bpv7::ValidBundle::Rewritten(bundle, data, report_unsupported) => {
                // Write the bundle data to the store
                let (storage_name, hash) = self.store.store_data(&data).await?;
                self.ingress_bundle(
                    bundle::Bundle {
                        metadata: BundleMetadata {
                            storage_name: Some(storage_name),
                            hash: Some(hash),
                            received_at,
                            ..Default::default()
                        },
                        bundle,
                    },
                    None,
                    report_unsupported,
                )
            }
            bpv7::ValidBundle::Invalid(bundle, reason, e) => {
                trace!("Invalid bundle received: {e}");

                // Don't bother saving the bundle data, it's garbage
                self.ingress_bundle(
                    bundle::Bundle {
                        metadata: BundleMetadata {
                            status: BundleStatus::Tombstone(time::OffsetDateTime::now_utc()),
                            received_at,
                            ..Default::default()
                        },
                        bundle,
                    },
                    Some(reason),
                    false,
                )
            }
        }
        .await
        .map_err(Into::into)
    }

    #[instrument(skip(self))]
    pub async fn ingress_bundle(
        &self,
        bundle: bundle::Bundle,
        reason: Option<bpv7::StatusReportReasonCode>,
        report_unsupported: bool,
    ) -> Result<(), Error> {
        // Report we have received the bundle
        let mut r = self
            .report_bundle_reception(
                &bundle,
                bpv7::StatusReportReasonCode::NoAdditionalInformation,
            )
            .await;

        // Report anything unsupported
        if r.is_ok() && report_unsupported {
            r = self
                .report_bundle_reception(&bundle, bpv7::StatusReportReasonCode::BlockUnsupported)
                .await;
        }

        /* RACE: If there is a crash between the report creation(above) and the metadata store (below)
         *  then we may send more than one "Received" Status Report when restarting,
         *  but that is currently considered benign (as a duplicate report causes little harm)
         *  and unlikely (as the report forwarding process is expected to take longer than the metadata.store)
         */

        if r.is_ok() {
            r = match self
                .store
                .store_metadata(&bundle.metadata, &bundle.bundle)
                .await
            {
                Ok(true) => Ok(()),
                Ok(false) => {
                    // Bundle with matching id already exists in the metadata store
                    trace!("Bundle with matching id already exists in the metadata store");

                    // Drop the stored data if it was valid, and do not process further
                    if let Some(storage_name) = &bundle.metadata.storage_name {
                        self.store.delete_data(storage_name).await?;
                    }
                    return Ok(());
                }
                Err(e) => Err(e),
            };
        }

        let storage_name = bundle.metadata.storage_name.clone();
        if r.is_ok() {
            // Check the bundle further
            r = self.check_bundle(bundle, reason).await;
        }

        if r.is_err() {
            // Drop the stored data if it was valid, and do not process further
            if let Some(storage_name) = &storage_name {
                self.store.delete_data(storage_name).await?;
            }
        }
        r
    }

    #[instrument(skip(self))]
    pub async fn check_bundle(
        &self,
        bundle: bundle::Bundle,
        mut reason: Option<bpv7::StatusReportReasonCode>,
    ) -> Result<(), Error> {
        /* Always check bundles, no matter the state, as after restarting
         * the configured filters or code may have changed, and reprocessing is desired.
         */

        if bundle.bundle.flags.unrecognised != 0 {
            trace!(
                "Bundle primary block has unrecognised flag bits set: {:#x}",
                bundle.bundle.flags.unrecognised
            );
        }

        if reason.is_none() {
            // Check some basic semantic validity, lifetime first
            if bundle.has_expired() {
                trace!("Bundle lifetime has expired");
                reason = Some(bpv7::StatusReportReasonCode::LifetimeExpired);
            } else if let Some(hop_info) = bundle.bundle.hop_count.as_ref() {
                // Check hop count exceeded
                if hop_info.count >= hop_info.limit {
                    trace!(
                        "Bundle hop-limit {}/{} exceeded",
                        hop_info.count,
                        hop_info.limit
                    );
                    reason = Some(bpv7::StatusReportReasonCode::HopLimitExceeded);
                }
            }
        }

        if reason.is_some() {
            // Not valid, drop it
            return self.drop_bundle(bundle, reason).await;
        }

        // Now process in parallel
        self.dispatch_bundle(bundle).await
    }
}
