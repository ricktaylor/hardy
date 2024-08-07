use super::*;
use hardy_bpa_api::storage;
use hardy_cbor as cbor;
use std::sync::Arc;

pub struct Ingress {
    store: Arc<store::Store>,
    dispatcher: Arc<dispatcher::Dispatcher>,
}

impl Ingress {
    pub fn new(
        _config: &config::Config,
        store: Arc<store::Store>,
        dispatcher: Arc<dispatcher::Dispatcher>,
    ) -> Arc<Self> {
        Arc::new(Self { store, dispatcher })
    }

    #[instrument(skip(self))]
    pub async fn receive(&self, data: Box<[u8]>) -> Result<(), Error> {
        // We need an Arc<[T]>
        let data: Arc<[u8]> = Arc::from(data);

        // Capture received_at as soon as possible
        let received_at = time::OffsetDateTime::now_utc();

        // Write the bundle data to the store
        let (storage_name, hash) = self.store.store_data(data.clone()).await?;

        // Put bundle into receive channel
        self.receive_bundle(
            storage_name,
            hash,
            store::into_dataref(data),
            Some(received_at),
        )
        .await
    }

    #[instrument(skip(self, data))]
    pub async fn receive_bundle(
        &self,
        storage_name: Arc<str>,
        hash: Arc<[u8]>,
        data: storage::DataRef,
        received_at: Option<time::OffsetDateTime>,
    ) -> Result<(), Error> {
        let (bundle, mut valid) =
            match cbor::decode::parse::<bpv7::ValidBundle>(data.as_ref().as_ref()) {
                Ok(bpv7::ValidBundle::Valid(bundle)) => (bundle, true),
                Ok(bpv7::ValidBundle::Invalid(bundle)) => (bundle, false),
                Err(e) => {
                    // Parse failed badly, no idea who to report to
                    trace!("Bundle parsing failed: {e}");

                    // Drop the bundle
                    return self.store.delete_data(&storage_name).await;
                }
            };

        // We are done with data, save heap space
        drop(data);

        let bundle = metadata::Bundle {
            metadata: metadata::Metadata {
                status: metadata::BundleStatus::IngressPending,
                storage_name,
                hash,
                received_at,
            },
            bundle,
        };

        // Report we have received the bundle
        self.dispatcher
            .report_bundle_reception(
                &bundle,
                bpv7::StatusReportReasonCode::NoAdditionalInformation,
            )
            .await?;

        /* RACE: If there is a crash between the report creation(above) and the metadata store (below)
         *  then we may send more than one "Received" Status Report when restarting,
         *  but that is currently considered benign (as a duplicate report causes little harm)
         *  and unlikely (as the report forwarding process is expected to take longer than the metadata.store)
         */

        // Store the bundle metadata in the store
        if !valid {
            trace!("Bundle is unintelligible");
        } else if !self
            .store
            .store_metadata(&bundle.metadata, &bundle.bundle)
            .await?
        {
            // Bundle with matching id already exists in the metadata store
            trace!("Bundle with matching id already exists in the metadata store");

            valid = false;
        }

        // Not valid, drop it
        if !valid {
            self.dispatcher
                .report_bundle_deletion(&bundle, bpv7::StatusReportReasonCode::BlockUnintelligible)
                .await?;

            // Drop the bundle
            return self.store.delete_data(&bundle.metadata.storage_name).await;
        }

        // Process the bundle further
        self.process_bundle(bundle).await
    }

    #[instrument(skip(self))]
    pub async fn process_bundle(&self, mut bundle: metadata::Bundle) -> Result<(), Error> {
        /* Always check bundles, no matter the state, as after restarting
        the configured filters may have changed, and reprocessing is desired. */

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
            // TODO: BPSec here!
        }

        if reason.is_none() {
            // TODO: Pluggable Ingress filters!
        }

        // Check extension blocks - do this last as it can rewrite the bundle
        if reason.is_none() {
            (reason, bundle) = self.check_extension_blocks(bundle).await?;
        }

        if let Some(reason) = reason {
            // Not valid, drop it
            return self.dispatcher.drop_bundle(bundle, Some(reason)).await;
        }

        if let metadata::BundleStatus::IngressPending = &bundle.metadata.status {
            // Update the status
            bundle.metadata.status = metadata::BundleStatus::DispatchPending;
            self.store
                .set_status(&bundle.metadata.storage_name, &bundle.metadata.status)
                .await?;
        }

        // Just pass it on to the dispatcher to deal with
        self.dispatcher.dispatch_bundle(bundle).await
    }

    async fn check_extension_blocks(
        &self,
        mut bundle: metadata::Bundle,
    ) -> Result<(Option<bpv7::StatusReportReasonCode>, metadata::Bundle), Error> {
        // Check for unsupported block types
        let mut blocks_to_remove = Vec::new();

        for (block_number, block) in &bundle.bundle.blocks {
            if let bpv7::BlockType::Private(_) = &block.block_type {
                if block.flags.report_on_failure {
                    self.dispatcher
                        .report_bundle_reception(
                            &bundle,
                            bpv7::StatusReportReasonCode::BlockUnsupported,
                        )
                        .await?;
                }

                if block.flags.delete_bundle_on_failure {
                    return Ok((Some(bpv7::StatusReportReasonCode::BlockUnsupported), bundle));
                }

                if block.flags.delete_block_on_failure {
                    blocks_to_remove.push(*block_number);
                }
            }
        }

        // Rewrite bundle if needed
        if !blocks_to_remove.is_empty() {
            let mut editor = bpv7::Editor::new(&bundle.bundle);
            for block_number in blocks_to_remove {
                editor = editor.remove_extension_block(block_number);
            }

            // Load up the source bundle data
            let Some(source_data) = self.store.load_data(&bundle.metadata.storage_name).await?
            else {
                // Bundle data was deleted sometime during processing
                return Ok((Some(bpv7::StatusReportReasonCode::DepletedStorage), bundle));
            };

            // Edit the bundle
            let (new_bundle, data) = editor.build((*source_data).as_ref())?;

            // Replace in store
            let metadata = self.store.replace_data(&bundle.metadata, data).await?;

            bundle = metadata::Bundle {
                metadata,
                bundle: new_bundle,
            };
        }
        Ok((None, bundle))
    }
}
