use super::*;
use tokio::sync::mpsc::*;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClaAddress {
    pub protocol: String,
    pub name: String,
    pub address: Vec<u8>,
}

#[derive(Clone)]
struct Config {
    allow_null_sources: bool,
}

impl Config {
    fn load(config: &config::Config) -> Result<Self, anyhow::Error> {
        Ok(Self {
            allow_null_sources: settings::get_with_default(config, "allow_null_sources", false)?,
        })
    }
}

#[derive(Clone)]
pub struct Ingress {
    config: Config,
    store: store::Store,
    receive_channel: Sender<(Option<ClaAddress>, String, Option<time::OffsetDateTime>)>,
    restart_channel: Sender<(bundle::Metadata, bundle::Bundle)>,
    dispatcher: dispatcher::Dispatcher,
    fib: Option<fib::Fib>,
}

impl Ingress {
    pub fn new(
        config: &config::Config,
        store: store::Store,
        dispatcher: dispatcher::Dispatcher,
        fib: Option<fib::Fib>,
        task_set: &mut tokio::task::JoinSet<()>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<Self, anyhow::Error> {
        // Load config
        let config = Config::load(config)?;

        // Create a channel for new bundles
        let (receive_channel, receive_channel_rx) = channel(16);
        let (ingress_channel, ingress_channel_rx) = channel(16);
        let ingress = Self {
            config,
            store,
            receive_channel,
            restart_channel: ingress_channel,
            dispatcher,
            fib,
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

    #[instrument(skip(self))]
    pub async fn receive(
        &self,
        from: Option<ClaAddress>,
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

    pub async fn recheck_bundle(
        &self,
        metadata: bundle::Metadata,
        bundle: bundle::Bundle,
    ) -> Result<(), anyhow::Error> {
        // Put bundle into ingress channel
        self.restart_channel
            .send((metadata, bundle))
            .await
            .map_err(|e| e.into())
    }

    #[instrument(skip_all)]
    async fn pipeline_pump(
        self,
        mut receive_channel: Receiver<(Option<ClaAddress>, String, Option<time::OffsetDateTime>)>,
        mut restart_channel: Receiver<(bundle::Metadata, bundle::Bundle)>,
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
                        let cancel_token_cloned = cancel_token.clone();
                        task_set.spawn(async move {
                            ingress.receive_bundle(cla_source,storage_name,received_at,cancel_token_cloned).await.log_expect("Failed to process received bundle")
                        });
                    }
                },
                msg = restart_channel.recv() => match msg {
                    None => break,
                    Some((metadata,bundle)) => {
                        let ingress = self.clone();
                        let cancel_token_cloned = cancel_token.clone();
                        task_set.spawn(async move {
                            ingress.process_bundle(None,metadata,bundle,cancel_token_cloned).await.log_expect("Failed to process restart bundle")
                        });
                    }
                },
                Some(r) = task_set.join_next() => r.log_expect("Task terminated unexpectedly"),
                _ = cancel_token.cancelled() => break
            }
        }

        // Wait for all sub-tasks to complete
        while let Some(r) = task_set.join_next().await {
            r.log_expect("Task terminated unexpectedly")
        }
    }

    #[instrument(skip(self))]
    async fn receive_bundle(
        &self,
        from: Option<ClaAddress>,
        storage_name: String,
        received_at: Option<time::OffsetDateTime>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<(), anyhow::Error> {
        // Parse the bundle
        let (metadata, bundle, valid) = {
            let data = self.store.load_data(&storage_name).await?;
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
                    log::trace!("Bundle parsing failed: {}", e);
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
            log::trace!("Bundle is unintelligible");

            // Not valid, drop it
            self.dispatcher
                .report_bundle_deletion(
                    &metadata,
                    &bundle,
                    bundle::StatusReportReasonCode::BlockUnintelligible,
                )
                .await?;

            // Drop the bundle
            log::trace!("Deleting bundle");
            return self.store.delete(&metadata.storage_name).await;
        }

        // Process the bundle further
        self.process_bundle(from, metadata, bundle, cancel_token)
            .await
    }

    #[instrument(skip(self))]
    async fn process_bundle(
        &self,
        from: Option<ClaAddress>,
        mut metadata: bundle::Metadata,
        mut bundle: bundle::Bundle,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<(), anyhow::Error> {
        /* Always check bundles,  no matter the state, as after restarting
        the configured filters may have changed, and reprocessing is desired. */

        // Check some basic semantic validity, lifetime first
        let mut reason = bundle::has_bundle_expired(&metadata, &bundle)
            .then(|| {
                log::trace!("Bundle lifetime has expired");
                bundle::StatusReportReasonCode::LifetimeExpired
            })
            .or_else(|| {
                // Check hop count exceeded
                bundle.hop_count.and_then(|hop_info| {
                    (hop_info.count >= hop_info.limit).then(|| {
                        log::trace!(
                            "Bundle hop-limit {}/{} exceeded",
                            hop_info.count,
                            hop_info.limit
                        );
                        bundle::StatusReportReasonCode::HopLimitExceeded
                    })
                })
            })
            .or_else(|| {
                // Source Eid checks
                match bundle.id.source {
                    hardy_bpa_core::bundle::Eid::Null => {
                        log::trace!("Bundle has Null source");
                        self.config
                            .allow_null_sources
                            .then_some(bundle::StatusReportReasonCode::BlockUnintelligible)
                    }
                    hardy_bpa_core::bundle::Eid::LocalNode { service_number: _ } => {
                        log::trace!("Bundle has LocalNode");
                        Some(bundle::StatusReportReasonCode::BlockUnintelligible)
                    }
                    _ => None,
                }
            })
            .or_else(|| {
                // Do the constant checks only on ingress bundles
                if let bundle::BundleStatus::IngressPending = &metadata.status {
                    // Destination Eid checks
                    match bundle.destination {
                        hardy_bpa_core::bundle::Eid::Null => {
                            log::trace!("Bundle has Null destination");
                            Some(bundle::StatusReportReasonCode::BlockUnintelligible)
                        }
                        hardy_bpa_core::bundle::Eid::LocalNode { service_number: _ } => {
                            log::trace!("Bundle has LocalNode destination");
                            Some(bundle::StatusReportReasonCode::BlockUnintelligible)
                        }
                        _ => None,
                    }
                    .or_else(|| {
                        // Report-To Eid checks
                        if let hardy_bpa_core::bundle::Eid::LocalNode { service_number: _ } =
                            bundle.report_to
                        {
                            log::trace!("Bundle has LocalNode report-to");
                            Some(bundle::StatusReportReasonCode::BlockUnintelligible)
                        } else {
                            None
                        }
                    })
                } else {
                    None
                }
            });

        if reason.is_none() {
            /* By the time we get here, we are pretty confident that the bundle isn't garbage
             * So we can confidently add routes if forwarding is enabled */
            if let (Some(from), Some(fib)) = (&from, &self.fib) {
                // Record a route to 'from.address' via 'from.name'
                let _ = fib.add(
                    fib::Destination::Cla(from.clone()),
                    0,
                    fib::Action::Forward {
                        protocol: from.protocol.clone(),
                        address: from.address.clone(),
                    },
                );

                // Record a route to 'previous_node' via 'from.address'
                let _ = bundle
                    .previous_node
                    .clone()
                    .unwrap_or(bundle.id.source.clone())
                    .try_into()
                    .map(|p| fib.add(p, 0, fib::Action::Via(fib::Destination::Cla(from.clone()))));
            }
        }

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
            log::trace!("Discarding bundle, leaving tombstone");
            return self.store.remove(&metadata.storage_name).await;
        }

        if let bundle::BundleStatus::IngressPending = &metadata.status {
            // Update the status
            metadata.status = self
                .store
                .set_status(
                    &metadata.storage_name,
                    bundle::BundleStatus::DispatchPending,
                )
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
        anyhow::Error,
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
            let mut editor = bundle::Editor::new(metadata, bundle);
            for block_number in blocks_to_remove {
                editor = editor.remove_extension_block(block_number);
            }
            (metadata, bundle) = editor.build(&self.store).await?;
        }
        Ok((None, metadata, bundle))
    }
}
