use super::*;
use hardy_bpa_core::storage;
use sha2::Digest;
use tokio::sync::mpsc::*;

pub struct ClaSource {
    pub protocol: String,
    pub address: Vec<u8>,
}

pub struct Ingress {
    store: store::Store,
    reassembler: reassembler::Reassembler,
    dispatcher: dispatcher::Dispatcher,
    receive_channel: Sender<(Option<ClaSource>, String, Option<time::OffsetDateTime>)>,
    ingress_channel: Sender<(bundle::Metadata, bundle::Bundle)>,
}

impl Clone for Ingress {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            dispatcher: self.dispatcher.clone(),
            reassembler: self.reassembler.clone(),
            receive_channel: self.receive_channel.clone(),
            ingress_channel: self.ingress_channel.clone(),
        }
    }
}

impl Ingress {
    pub fn new(
        _config: &config::Config,
        store: store::Store,
        reassembler: reassembler::Reassembler,
        dispatcher: dispatcher::Dispatcher,
        task_set: &mut tokio::task::JoinSet<()>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<Self, anyhow::Error> {
        // Create a channel for new bundles
        let (receive_channel, receive_channel_rx) = channel(16);
        let (ingress_channel, ingress_channel_rx) = channel(16);
        let ingress = Self {
            store,
            reassembler,
            dispatcher,
            receive_channel,
            ingress_channel,
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

        Ok(ingress)
    }

    pub async fn receive(
        &self,
        from: Option<ClaSource>,
        data: Vec<u8>,
    ) -> Result<(), anyhow::Error> {
        // Capture received_at as soon as possible
        let received_at = time::OffsetDateTime::now_utc();

        // Write the bundle data to the store
        let storage_name = self.store.store_data(data).await?;

        // Put bundle into receive channel
        self.receive_channel
            .send((from, storage_name, Some(received_at)))
            .await
            .map_err(|e| e.into())
    }

    pub async fn enqueue_receive_bundle(
        &self,
        storage_name: &str,
        received_at: Option<time::OffsetDateTime>,
    ) -> Result<(), anyhow::Error> {
        // Put bundle into receive channel
        self.receive_channel
            .send((None, storage_name.to_string(), received_at))
            .await
            .map_err(|e| e.into())
    }

    pub async fn enqueue_ingress_bundle(
        &self,
        metadata: bundle::Metadata,
        bundle: bundle::Bundle,
    ) -> Result<(), anyhow::Error> {
        // Put bundle into ingress channel
        self.ingress_channel
            .send((metadata, bundle))
            .await
            .map_err(|e| e.into())
    }

    async fn pipeline_pump(
        self,
        mut receive_channel: Receiver<(Option<ClaSource>, String, Option<time::OffsetDateTime>)>,
        mut ingress_channel: Receiver<(bundle::Metadata, bundle::Bundle)>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        // We're going to spawn a bunch of tasks
        let mut task_set = tokio::task::JoinSet::new();

        loop {
            tokio::select! {
                msg = receive_channel.recv() => match msg {
                    None => break,
                    Some((cla_source,storage_name,received_at)) => {
                        let ingress = self.clone();
                        task_set.spawn(async move {
                            ingress.process_receive_bundle(cla_source,storage_name,received_at).await.log_expect("Failed to process received bundle")
                        });
                    }
                },
                msg = ingress_channel.recv() => match msg {
                    None => break,
                    Some((metadata,bundle)) => {
                        let ingress = self.clone();
                        task_set.spawn(async move {
                            ingress.process_ingress_bundle(metadata,bundle).await.log_expect("Failed to process ingress bundle")
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

    async fn process_receive_bundle(
        &self,
        from: Option<ClaSource>,
        storage_name: String,
        received_at: Option<time::OffsetDateTime>,
    ) -> Result<(), anyhow::Error> {
        // Parse the bundle
        let (metadata, bundle, valid) = {
            let data = self.store.load_data(&storage_name).await?;
            match bundle::parse_bundle((*data).as_ref()) {
                Ok((bundle, valid)) => (
                    bundle::Metadata {
                        status: bundle::BundleStatus::IngressPending,
                        storage_name,
                        hash: sha2::Sha256::digest(data.as_ref()).to_vec(),
                        received_at,
                    },
                    bundle,
                    valid,
                ),
                Err(e) => {
                    // Parse failed badly, no idea who to report to
                    log::info!("Bundle parsing failed: {}", e);
                    return Ok(());
                }
            }
        };

        // Report we have received the bundle
        self.dispatcher
            .report_bundle_reception(
                &metadata,
                &bundle,
                dispatcher::BundleStatusReportReasonCode::NoAdditionalInformation,
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
            // Not valid, drop it
            self.dispatcher
                .report_bundle_deletion(
                    &metadata,
                    &bundle,
                    dispatcher::BundleStatusReportReasonCode::BlockUnintelligible,
                )
                .await?;

            // Drop the bundle
            return self.store.remove(&metadata.storage_name).await;
        }

        if let Some(_from) = from {
            // TODO: Try to learn a route from `from`
        }

        // Process the bundle further
        self.process_ingress_bundle(metadata, bundle).await
    }

    async fn process_ingress_bundle(
        &self,
        mut metadata: bundle::Metadata,
        mut bundle: bundle::Bundle,
    ) -> Result<(), anyhow::Error> {
        loop {
            // Duff's device
            let reason = match &metadata.status {
                bundle::BundleStatus::IngressPending => {
                    /* RACE: If there is a crash between the report creation(above) and the status update (below)
                     * then we may send more than one "Received" Status Report, but that is currently considered benign and unlikely ;)
                     */

                    // Check bundle blocks
                    let reason = self.check_bundle_blocks(&mut metadata, &mut bundle).await?;

                    // TODO: More here!

                    if reason.is_none() {
                        metadata.status = if bundle.id.fragment_info.is_some() {
                            // Fragments require reassembly
                            bundle::BundleStatus::ReassemblyPending
                        } else {
                            // Dispatch!
                            bundle::BundleStatus::DispatchPending
                        };
                    }
                    reason
                }
                bundle::BundleStatus::ReassemblyPending => {
                    // Send on to the reassembler
                    return self.reassembler.enqueue_bundle(metadata, bundle).await;
                }
                _ => {
                    // Just send it on to the dispatcher to deal with
                    return self.dispatcher.enqueue_bundle(metadata, bundle).await;
                }
            };

            if let Some(reason) = reason {
                // Not valid, drop it
                self.dispatcher
                    .report_bundle_deletion(&metadata, &bundle, reason)
                    .await?;

                // Drop the bundle
                return self.store.remove(&metadata.storage_name).await;
            }

            // Update the status
            self.store
                .set_status(&metadata.storage_name, metadata.status)
                .await?;
        }
    }

    async fn check_bundle_blocks(
        &self,
        metadata: &mut bundle::Metadata,
        bundle: &mut bundle::Bundle,
    ) -> Result<Option<dispatcher::BundleStatusReportReasonCode>, anyhow::Error> {
        // Check for supported block types
        let mut seen_payload = false;
        let mut seen_previous_node = false;
        let mut seen_bundle_age = false;
        let mut seen_hop_count = false;
        let mut blocks_to_remove = Vec::new();

        for (block_number, block) in &bundle.blocks {
            let (supported, valid) = match &block.block_type {
                bundle::BlockType::Payload => (
                    true,
                    if seen_payload {
                        log::info!("Bundle has multiple payload blocks");
                        false
                    } else if *block_number != 1 {
                        log::info!("Bundle has payload block with number {}", block_number);
                        false
                    } else {
                        seen_payload = true;
                        true
                    },
                ),
                bundle::BlockType::PreviousNode => (
                    true,
                    if seen_previous_node {
                        log::info!("Bundle has multiple Previous Node extension blocks");
                        false
                    } else {
                        seen_previous_node = true;
                        self.check_previous_node(metadata, block)?
                    },
                ),
                bundle::BlockType::BundleAge => (
                    true,
                    if seen_bundle_age {
                        log::info!("Bundle has multiple Bundle Age extension blocks");
                        false
                    } else {
                        seen_bundle_age = true;
                        self.check_bundle_age(metadata, block)?
                    },
                ),
                bundle::BlockType::HopCount => (
                    true,
                    if seen_hop_count {
                        log::info!("Bundle has multiple Hop Count extension blocks");
                        false
                    } else {
                        seen_hop_count = true;
                        self.check_hop_count(metadata, block)?
                    },
                ),
                bundle::BlockType::Private(_) => (false, true),
            };

            if !valid {
                return Ok(Some(
                    dispatcher::BundleStatusReportReasonCode::BlockUnintelligible,
                ));
            }

            if !supported {
                if block.flags.report_on_failure {
                    self.dispatcher
                        .report_bundle_reception(
                            metadata,
                            bundle,
                            dispatcher::BundleStatusReportReasonCode::BlockUnsupported,
                        )
                        .await?;
                }

                if block.flags.delete_bundle_on_failure {
                    return Ok(Some(
                        dispatcher::BundleStatusReportReasonCode::BlockUnsupported,
                    ));
                }

                if block.flags.delete_block_on_failure {
                    blocks_to_remove.push(block_number);
                }
            }
        }

        if !seen_bundle_age && bundle.id.timestamp.creation_time == 0 {
            log::info!("Bundle source had no clock, and there is no Bundle Age extension block");

            return Ok(Some(
                dispatcher::BundleStatusReportReasonCode::BlockUnintelligible,
            ));
        }

        if !blocks_to_remove.is_empty() {
            // Rewrite bundle!

            todo!()
        }

        Ok(None)
    }
}
