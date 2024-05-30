use super::*;
use tokio::sync::mpsc::*;

#[derive(Clone)]
pub struct Ingress {
    store: store::Store,
    receive_channel: Sender<(String, Option<time::OffsetDateTime>)>,
    restart_channel: Sender<(bundle::Metadata, bundle::Bundle)>,
    dispatcher: dispatcher::Dispatcher,
}

impl Ingress {
    pub fn new(
        _config: &config::Config,
        store: store::Store,
        dispatcher: dispatcher::Dispatcher,
        task_set: &mut tokio::task::JoinSet<()>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Self {
        // Create a channel for new bundles
        let (receive_channel, receive_channel_rx) = channel(16);
        let (ingress_channel, ingress_channel_rx) = channel(16);
        let ingress = Self {
            store,
            receive_channel,
            restart_channel: ingress_channel,
            dispatcher,
        };

        // Spawn a bundle receiver
        let ingress_cloned = ingress.clone();
        task_set.spawn(async move {
            Self::pipeline_pump(
                ingress_cloned,
                receive_channel_rx,
                ingress_channel_rx,
                cancel_token,
            )
            .await
        });

        ingress
    }

    #[instrument(skip(self))]
    pub async fn receive(&self, data: Vec<u8>) -> Result<(), Error> {
        // Capture received_at as soon as possible
        let received_at = time::OffsetDateTime::now_utc();

        // Write the bundle data to the store
        let storage_name = self.store.store_data(data).await?;

        // Put bundle into receive channel
        self.receive_channel
            .send((storage_name, Some(received_at)))
            .await
            .map_err(|e| e.into())
    }

    pub async fn enqueue_receive_bundle(
        &self,
        storage_name: &str,
        received_at: Option<time::OffsetDateTime>,
    ) -> Result<(), Error> {
        // Put bundle into receive channel
        self.receive_channel
            .send((storage_name.to_string(), received_at))
            .await
            .map_err(|e| e.into())
    }

    pub async fn recheck_bundle(
        &self,
        metadata: bundle::Metadata,
        bundle: bundle::Bundle,
    ) -> Result<(), Error> {
        // Put bundle into ingress channel
        self.restart_channel
            .send((metadata, bundle))
            .await
            .map_err(|e| e.into())
    }

    #[instrument(skip_all)]
    async fn pipeline_pump(
        self,
        mut receive_channel: Receiver<(String, Option<time::OffsetDateTime>)>,
        mut restart_channel: Receiver<(bundle::Metadata, bundle::Bundle)>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        // We're going to spawn a bunch of tasks
        let mut task_set = tokio::task::JoinSet::new();

        loop {
            tokio::select! {
                msg = receive_channel.recv() => match msg {
                    None => break,
                    Some((storage_name,received_at)) => {
                        let ingress = self.clone();
                        let cancel_token_cloned = cancel_token.clone();
                        task_set.spawn(async move {
                            ingress.receive_bundle(storage_name,received_at,cancel_token_cloned).await.trace_expect("Failed to process received bundle")
                        });
                    }
                },
                msg = restart_channel.recv() => match msg {
                    None => break,
                    Some((metadata,bundle)) => {
                        let ingress = self.clone();
                        let cancel_token_cloned = cancel_token.clone();
                        task_set.spawn(async move {
                            ingress.process_bundle(metadata,bundle,cancel_token_cloned).await.trace_expect("Failed to process restart bundle")
                        });
                    }
                },
                Some(r) = task_set.join_next() => r.trace_expect("Task terminated unexpectedly"),
                _ = cancel_token.cancelled() => break
            }
        }

        // Wait for all sub-tasks to complete
        while let Some(r) = task_set.join_next().await {
            r.trace_expect("Task terminated unexpectedly")
        }
    }

    #[instrument(skip(self))]
    async fn receive_bundle(
        &self,
        storage_name: String,
        received_at: Option<time::OffsetDateTime>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<(), Error> {
        // Parse the bundle
        let (metadata, bundle, valid) = {
            let Some(data) = self.store.load_data(&storage_name).await? else {
                // Bundle data was deleted sometime during processing
                return Ok(());
            };

            match bundle::parse((*data).as_ref()) {
                Ok((bundle, valid)) => (
                    bundle::Metadata {
                        status: bundle::BundleStatus::IngressPending,
                        storage_name,
                        hash: self.store.hash((*data).as_ref()),
                        received_at,
                    },
                    bundle,
                    valid,
                ),
                Err(e) => {
                    // Parse failed badly, no idea who to report to
                    trace!("Bundle parsing failed: {e}");
                    return Ok(());
                }
            }
        };

        // Report we have received the bundle
        self.dispatcher
            .report_bundle_reception(
                &metadata,
                &bundle,
                bundle::StatusReportReasonCode::NoAdditionalInformation,
            )
            .await?;

        /* RACE: If there is a crash between the report creation(above) and the metadata store (below)
         *  then we may send more than one "Received" Status Report when restarting,
         *  but that is currently considered benign (as a duplicate report causes little harm)
         *  and unlikely (as the report forwarding process is expected to take longer than the metadata.store)
         */

        // Store the bundle metadata in the store
        self.store.store_metadata(&metadata, &bundle).await?;

        if !valid {
            trace!("Bundle is unintelligible");

            // Not valid, drop it
            self.dispatcher
                .report_bundle_deletion(
                    &metadata,
                    &bundle,
                    bundle::StatusReportReasonCode::BlockUnintelligible,
                )
                .await?;

            // Drop the bundle
            trace!("Deleting bundle");
            return self.store.delete(&metadata.storage_name).await;
        }

        // Process the bundle further
        self.process_bundle(metadata, bundle, cancel_token).await
    }

    #[instrument(skip(self))]
    async fn process_bundle(
        &self,
        mut metadata: bundle::Metadata,
        mut bundle: bundle::Bundle,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<(), Error> {
        /* Always check bundles,  no matter the state, as after restarting
        the configured filters may have changed, and reprocessing is desired. */

        // Check some basic semantic validity, lifetime first
        let mut reason = bundle::has_bundle_expired(&metadata, &bundle)
            .then(|| {
                trace!("Bundle lifetime has expired");
                bundle::StatusReportReasonCode::LifetimeExpired
            })
            .or_else(|| {
                // Check hop count exceeded
                bundle.hop_count.and_then(|hop_info| {
                    (hop_info.count >= hop_info.limit).then(|| {
                        trace!(
                            "Bundle hop-limit {}/{} exceeded",
                            hop_info.count,
                            hop_info.limit
                        );
                        bundle::StatusReportReasonCode::HopLimitExceeded
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
            (reason, metadata, bundle) = self.check_extension_blocks(metadata, bundle).await?;
        }

        if let Some(reason) = reason {
            // Not valid, drop it
            self.dispatcher
                .report_bundle_deletion(&metadata, &bundle, reason)
                .await?;

            // Drop the bundle
            trace!("Discarding bundle, leaving tombstone");
            return self.store.remove(&metadata.storage_name).await;
        }

        if let bundle::BundleStatus::IngressPending = &metadata.status {
            // Update the status
            metadata.status = bundle::BundleStatus::DispatchPending;
            self.store
                .set_status(&metadata.storage_name, &metadata.status)
                .await?;
        }

        // Just pass it on to the dispatcher to deal with
        self.dispatcher
            .process_bundle(metadata, bundle, cancel_token)
            .await
    }

    async fn check_extension_blocks(
        &self,
        mut metadata: bundle::Metadata,
        mut bundle: bundle::Bundle,
    ) -> Result<
        (
            Option<bundle::StatusReportReasonCode>,
            bundle::Metadata,
            bundle::Bundle,
        ),
        Error,
    > {
        // Check for unsupported block types
        let mut blocks_to_remove = Vec::new();

        for (block_number, block) in &bundle.blocks {
            match &block.block_type {
                bundle::BlockType::PreviousNode | bundle::BlockType::BundleAge => {
                    // Always remove the Previous Node and Bundle Age blocks, as we have the data recorded
                    // And we must replace them before forwarding anyway
                    blocks_to_remove.push(*block_number);
                }
                bundle::BlockType::Private(_) => {
                    if block.flags.report_on_failure {
                        self.dispatcher
                            .report_bundle_reception(
                                &metadata,
                                &bundle,
                                bundle::StatusReportReasonCode::BlockUnsupported,
                            )
                            .await?;
                    }

                    if block.flags.delete_bundle_on_failure {
                        return Ok((
                            Some(bundle::StatusReportReasonCode::BlockUnsupported),
                            metadata,
                            bundle,
                        ));
                    }

                    if block.flags.delete_block_on_failure {
                        blocks_to_remove.push(*block_number);
                    }
                }
                _ => (),
            }
        }

        // Rewrite bundle if needed
        if !blocks_to_remove.is_empty() {
            let mut editor = bundle::Editor::new(&bundle);
            for block_number in blocks_to_remove {
                editor = editor.remove_extension_block(block_number);
            }

            // Load up the source bundle data
            let Some(source_data) = self.store.load_data(&metadata.storage_name).await? else {
                // Bundle data was deleted sometime during processing
                return Ok((
                    Some(bundle::StatusReportReasonCode::DepletedStorage),
                    metadata,
                    bundle,
                ));
            };

            // Edit the bundle
            let data;
            (bundle, data) = editor.build((*source_data).as_ref())?;

            // Replace in store
            metadata = self.store.replace_data(&metadata, data).await?;
        }
        Ok((None, metadata, bundle))
    }

    #[instrument(skip(self))]
    pub async fn confirm_forwarding(
        &self,
        handle: u32,
        bundle_id: &str,
    ) -> Result<(), tonic::Status> {
        let Some((metadata, bundle)) = self
            .store
            .load(
                &bundle::BundleId::from_key(bundle_id)
                    .map_err(|e| tonic::Status::from_error(e.into()))?,
            )
            .await
            .map_err(tonic::Status::from_error)?
        else {
            return Err(tonic::Status::not_found("No such bundle"));
        };

        match &metadata.status {
            bundle::BundleStatus::ForwardAckPending(t, _) if t == &handle => {
                // Report bundle forwarded
                self.dispatcher
                    .report_bundle_forwarded(&metadata, &bundle)
                    .await
                    .map_err(tonic::Status::from_error)?;

                // And tombstone the bundle
                self.store
                    .remove(&metadata.storage_name)
                    .await
                    .map_err(tonic::Status::from_error)
            }
            _ => Err(tonic::Status::not_found("No such bundle")),
        }
    }
}
