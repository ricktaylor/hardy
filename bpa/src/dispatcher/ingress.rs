use super::*;
use core::ops::Deref;
use hardy_bpv7::status_report::ReasonCode;

impl Dispatcher {
    #[instrument(level = "trace", skip_all)]
    pub async fn receive_bundle(self: &Arc<Self>, data: Bytes) -> cla::Result<()> {
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

    #[instrument(level = "trace", skip(self))]
    pub async fn ingress_bundle(
        self: &Arc<Self>,
        bundle: bundle::Bundle,
        mut reason: Option<ReasonCode>,
    ) -> Result<(), Error> {
        /* Always check bundles, no matter the state, as after restarting
         * the configured filters or code may have changed, and reprocessing is desired.
         */

        // Drop Eid::Null silently to cull spam
        if bundle.bundle.destination == Eid::Null {
            return self.drop_bundle(bundle, None).await;
        }

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
}
