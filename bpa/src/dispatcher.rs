use super::*;
use hardy_cbor as cbor;
use tokio::sync::mpsc::*;
use utils::{cancel::cancellable_sleep, settings};

// Defaults
const WAIT_SAMPLE_INTERVAL_SECS: u64 = 60;
const MAX_FORWARDING_DELAY_SECS: u32 = 5;

#[derive(Clone)]
struct Config {
    admin_endpoints: utils::admin_endpoints::AdminEndpoints,
    status_reports: bool,
    wait_sample_interval: u64,
    max_forwarding_delay: u32,
    ipn_2_element: bpv7::EidPatternMap<(), ()>,
}

impl Config {
    fn new(
        config: &config::Config,
        admin_endpoints: utils::admin_endpoints::AdminEndpoints,
    ) -> Self {
        let config = Self {
            admin_endpoints,
            status_reports: settings::get_with_default(config, "status_reports", false)
                .trace_expect("Invalid 'status_reports' value in configuration"),
            wait_sample_interval: settings::get_with_default(
                config,
                "wait_sample_interval",
                WAIT_SAMPLE_INTERVAL_SECS,
            )
            .trace_expect("Invalid 'wait_sample_interval' value in configuration"),
            max_forwarding_delay: settings::get_with_default::<u32, _>(
                config,
                "max_forwarding_delay",
                MAX_FORWARDING_DELAY_SECS,
            )
            .trace_expect("Invalid 'max_forwarding_delay' value in configuration")
            .min(1u32),
            ipn_2_element: Self::load_ipn_2_element(config),
        };

        if !config.status_reports {
            info!("Bundle status reports are disabled by configuration");
        }

        if config.max_forwarding_delay == 0 {
            info!("Forwarding synchronization delay disabled by configuration");
        }

        config
    }

    fn load_ipn_2_element(config: &config::Config) -> bpv7::EidPatternMap<(), ()> {
        let mut m = bpv7::EidPatternMap::new();
        for s in config
            .get::<Vec<String>>("ipn_2_element")
            .unwrap_or_default()
        {
            let p = s
                .parse::<bpv7::EidPattern>()
                .trace_expect(&format!("Invalid EID pattern '{s}"));
            m.insert(&p, (), ());
        }
        m
    }
}

#[derive(Default, Debug)]
pub struct SendRequest {
    pub source: bpv7::Eid,
    pub destination: bpv7::Eid,
    pub data: Vec<u8>,
    pub lifetime: Option<u64>,
    pub flags: Option<bpv7::BundleFlags>,
}

pub struct CollectResponse {
    pub bundle_id: String,
    pub expiry: time::OffsetDateTime,
    pub app_ack_requested: bool,
    pub data: Vec<u8>,
}

#[derive(Clone)]
pub struct Dispatcher {
    config: Config,
    store: store::Store,
    tx: Sender<metadata::Bundle>,
    cla_registry: cla_registry::ClaRegistry,
    app_registry: app_registry::AppRegistry,
    fib: Option<fib::Fib>,
}

impl Dispatcher {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: &config::Config,
        admin_endpoints: utils::admin_endpoints::AdminEndpoints,
        store: store::Store,
        cla_registry: cla_registry::ClaRegistry,
        app_registry: app_registry::AppRegistry,
        fib: Option<fib::Fib>,
        task_set: &mut tokio::task::JoinSet<()>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Self {
        // Create a channel for bundles
        let (tx, rx) = channel(16);
        let dispatcher = Self {
            config: Config::new(config, admin_endpoints),
            store,
            tx,
            cla_registry,
            app_registry,
            fib,
        };

        // Spawn a bundle receiver
        let dispatcher_cloned = dispatcher.clone();
        let cancel_token_cloned = cancel_token.clone();
        task_set.spawn(async move {
            Self::pipeline_pump(dispatcher_cloned, rx, cancel_token_cloned).await
        });

        // Spawn a waiter
        let dispatcher_cloned = dispatcher.clone();
        task_set.spawn(async move { Self::check_waiting(dispatcher_cloned, cancel_token).await });

        dispatcher
    }

    async fn enqueue_bundle(&self, bundle: metadata::Bundle) -> Result<(), Error> {
        // Put bundle into channel
        self.tx.send(bundle).await.map_err(|e| e.into())
    }

    #[instrument(skip_all)]
    async fn pipeline_pump(
        self,
        mut rx: Receiver<metadata::Bundle>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        // We're going to spawn a bunch of tasks
        let mut task_set = tokio::task::JoinSet::new();

        loop {
            tokio::select! {
                bundle = rx.recv() => match bundle {
                    None => break,
                    Some(bundle) => {
                        let dispatcher = self.clone();
                        let cancel_token_cloned = cancel_token.clone();
                        task_set.spawn(async move {
                            dispatcher.process_bundle(bundle,cancel_token_cloned).await.trace_expect("Failed to process bundle");
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

    #[instrument(skip_all)]
    async fn check_waiting(self, cancel_token: tokio_util::sync::CancellationToken) {
        let timer = tokio::time::sleep(tokio::time::Duration::from_secs(
            self.config.wait_sample_interval,
        ));
        tokio::pin!(timer);

        // We're going to spawn a bunch of tasks
        let mut task_set = tokio::task::JoinSet::new();

        loop {
            tokio::select! {
                () = &mut timer => {
                    // Determine next interval before we do any other waiting
                    let interval = tokio::time::Instant::now() + tokio::time::Duration::from_secs(self.config.wait_sample_interval);

                    // Get all bundles that are ready before now() + self.config.wait_sample_interval
                    let waiting = self.store.get_waiting_bundles(time::OffsetDateTime::now_utc() + time::Duration::new(self.config.wait_sample_interval as i64, 0)).await.trace_expect("get_waiting_bundles failed");
                    for (bundle,until) in waiting {
                        // Spawn a task for each ready bundle
                        let dispatcher = self.clone();
                        let cancel_token_cloned = cancel_token.clone();
                        task_set.spawn(async move {
                            dispatcher.delay_bundle(bundle, until,cancel_token_cloned).await.trace_expect("Failed to process bundle");
                        });
                    }

                    timer.as_mut().reset(interval);
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

    async fn delay_bundle(
        &self,
        mut bundle: metadata::Bundle,
        until: time::OffsetDateTime,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<(), Error> {
        // Check if it's worth us waiting (safety check for metadata storage clock drift)
        let wait = until - time::OffsetDateTime::now_utc();
        if wait > time::Duration::new(self.config.wait_sample_interval as i64, 0) {
            // Nothing to do now, it will be picked up later
            trace!("Bundle will wait offline until: {until}");
            return Ok(());
        }

        if let metadata::BundleStatus::ForwardAckPending(_, _) = &bundle.metadata.status {
            trace!("Waiting for bundle forwarding acknowledgement inline until: {until}");
        } else {
            trace!("Waiting to forward bundle inline until: {until}");
        }

        // Wait a bit
        if !cancellable_sleep(wait, &cancel_token).await {
            // Cancelled
            return Ok(());
        }

        if let metadata::BundleStatus::ForwardAckPending(_, _) = &bundle.metadata.status {
            // Check if the bundle has been acknowledged while we slept
            let Some(metadata::BundleStatus::ForwardAckPending(_, _)) = self
                .store
                .check_status(&bundle.metadata.storage_name)
                .await?
            else {
                // It's not longer waiting, our work here is done
                return Ok(());
            };
        }

        trace!("Forwarding bundle");

        // Set status to ForwardPending
        bundle.metadata.status = metadata::BundleStatus::ForwardPending;
        self.store
            .set_status(&bundle.metadata.storage_name, &bundle.metadata.status)
            .await?;

        // And forward it!
        self.forward_bundle(bundle, cancel_token).await
    }

    #[instrument(skip(self))]
    pub async fn process_bundle(
        &self,
        mut bundle: metadata::Bundle,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<(), Error> {
        if let metadata::BundleStatus::DispatchPending = &bundle.metadata.status {
            // Check if we are the final destination
            bundle.metadata.status = if self
                .config
                .admin_endpoints
                .is_local_service(&bundle.bundle.destination)
            {
                if bundle.bundle.id.fragment_info.is_some() {
                    // Reassembly!!
                    trace!("Bundle requires fragment reassembly");
                    metadata::BundleStatus::ReassemblyPending
                } else if self
                    .config
                    .admin_endpoints
                    .is_admin_endpoint(&bundle.bundle.destination)
                {
                    // The bundle is for the Administrative Endpoint
                    trace!("Bundle is destined for one of our administrative endpoints");
                    return self.administrative_bundle(bundle).await;
                } else {
                    // The bundle is ready for collection
                    trace!("Bundle is ready for local delivery");
                    metadata::BundleStatus::CollectionPending
                }
            } else {
                // Forward to another BPA
                trace!("Forwarding bundle");
                metadata::BundleStatus::ForwardPending
            };

            self.store
                .set_status(&bundle.metadata.storage_name, &bundle.metadata.status)
                .await?;
        }

        if let metadata::BundleStatus::ReassemblyPending = &bundle.metadata.status {
            // Attempt reassembly
            let Some(b) = self.reassemble(bundle).await? else {
                // Waiting for more fragments to arrive
                return Ok(());
            };
            bundle = b;
        }

        match &bundle.metadata.status {
            metadata::BundleStatus::IngressPending
            | metadata::BundleStatus::DispatchPending
            | metadata::BundleStatus::ReassemblyPending => {
                unreachable!()
            }
            metadata::BundleStatus::CollectionPending => {
                // Check if we have a local service registered
                if let Some(endpoint) = self.app_registry.find_by_eid(&bundle.bundle.destination) {
                    // Notify that the bundle is ready for collection
                    trace!("Notifying application that bundle is ready for collection");
                    endpoint.collection_notify(&bundle.bundle.id).await;
                }
                Ok(())
            }
            metadata::BundleStatus::ForwardAckPending(_, _) => {
                // Clear the pending ACK, we are reprocessing
                bundle.metadata.status = metadata::BundleStatus::ForwardPending;
                self.store
                    .set_status(&bundle.metadata.storage_name, &bundle.metadata.status)
                    .await?;

                // And just forward
                self.forward_bundle(bundle, cancel_token).await
            }
            metadata::BundleStatus::ForwardPending => {
                self.forward_bundle(bundle, cancel_token).await
            }
            metadata::BundleStatus::Waiting(until) => {
                let until = *until;
                self.delay_bundle(bundle, until, cancel_token).await
            }
            metadata::BundleStatus::Tombstone => Ok(()),
        }
    }

    async fn forward_bundle(
        &self,
        mut bundle: metadata::Bundle,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<(), Error> {
        let Some(fib) = &self.fib else {
            /* If forwarding is disabled in the configuration, then we can only deliver bundles.
             * As we have decided that the bundle is not for a local service, we cannot deliver.
             * Therefore, we respond with a Destination endpoint ID unavailable report */
            trace!("Bundle should be forwarded, but forwarding is disabled");
            return self
                .drop_bundle(
                    bundle,
                    Some(bpv7::StatusReportReasonCode::DestinationEndpointIDUnavailable),
                )
                .await;
        };

        // TODO: Pluggable Egress filters!

        /* We loop here, as the FIB could tell us that there should be a CLA to use to forward
         * But it might be rebooting or jammed, so we keep retrying for a "reasonable" amount of time */
        let mut data = None;
        let mut previous = false;
        let mut retries = 0;
        let mut destination = &bundle.bundle.destination;

        loop {
            // Check bundle expiry
            if bundle.has_expired() {
                trace!("Bundle lifetime has expired");
                return self
                    .drop_bundle(bundle, Some(bpv7::StatusReportReasonCode::LifetimeExpired))
                    .await;
            }

            // Lookup/Perform actions
            let action = match fib.find(destination).await {
                Err(reason) => {
                    trace!("Bundle is black-holed");
                    return self.drop_bundle(bundle, reason).await;
                }
                Ok(fib::ForwardAction {
                    clas,
                    wait: Some(wait),
                }) if clas.is_empty() => {
                    // Check to see if waiting is even worth it
                    if wait > bundle.expiry() {
                        trace!("Bundle lifetime is shorter than wait period");
                        return self
                                .drop_bundle(
                                    bundle,
                                    Some(
                                        bpv7::StatusReportReasonCode::NoTimelyContactWithNextNodeOnRoute,
                                    ),
                                )
                                .await;
                    }

                    // Wait a bit
                    if !self
                        .wait_to_forward(&bundle.metadata, wait, &cancel_token)
                        .await?
                    {
                        // Cancelled, or too long a wait for here
                        return Ok(());
                    }

                    // Reset retry counter, we were just correctly told to wait
                    retries = 0;
                    continue;
                }
                Ok(action) => action,
            };

            let mut data_is_time_sensitive = false;
            let mut congestion_wait = None;

            // For each CLA
            for endpoint in action.clas {
                // Find the named CLA
                if let Some(e) = self.cla_registry.find(endpoint.handle).await {
                    // Get bundle data from store, now we know we need it!
                    if data.is_none() {
                        let Some(source_data) = self.load_data(&bundle).await? else {
                            // Bundle data was deleted sometime during processing
                            return Ok(());
                        };

                        // Increment Hop Count, etc...
                        (data, data_is_time_sensitive) = self
                            .update_extension_blocks(&bundle, (*source_data).as_ref())
                            .map(|(data, data_is_time_sensitive)| {
                                (Some(data), data_is_time_sensitive)
                            })?;
                    }

                    match e.forward_bundle(destination, data.clone().unwrap()).await {
                        Ok((None, None)) => {
                            // We have successfully forwarded!
                            self.report_bundle_forwarded(&bundle).await?;
                            return self.drop_bundle(bundle, None).await;
                        }
                        Ok((Some(handle), until)) => {
                            // CLA will report successful forwarding
                            // Don't wait longer than expiry
                            let until = until.unwrap_or_else(|| {
                                warn!("CLA endpoint has not provided a suitable AckPending delay, defaulting to 1 minute");
                                time::OffsetDateTime::now_utc() + time::Duration::minutes(1)
                            }).min(bundle.expiry());

                            // Set the bundle status to 'Forward Acknowledgement Pending'
                            bundle.metadata.status =
                                metadata::BundleStatus::ForwardAckPending(handle, until);
                            return self
                                .store
                                .set_status(&bundle.metadata.storage_name, &bundle.metadata.status)
                                .await;
                        }
                        Ok((None, until)) => {
                            let until = until.unwrap_or_else(|| {
                                warn!("CLA endpoint has not provided a suitable Congestion delay, defaulting to 10 seconds");
                                time::OffsetDateTime::now_utc() + time::Duration::seconds(10)
                            });
                            trace!("CLA reported congestion, retry at: {until}");

                            // Remember the shortest wait for a retry, in case we have ECMP
                            congestion_wait = congestion_wait
                                .map_or(Some(until), |w: time::OffsetDateTime| Some(w.min(until)))
                        }
                        Err(e) => trace!("CLA failed to forward {e}"),
                    }
                } else {
                    trace!("FIB has entry for unknown CLA: {endpoint:?}");
                }
                // Try the next CLA, this one is busy, broken or missing
            }

            // By the time we get here, we have tried every CLA

            // Check for congestion
            if let Some(mut until) = congestion_wait {
                trace!("All available CLAs report congestion until {until}");

                // Limit congestion wait to the forwarding wait
                if let Some(wait) = action.wait {
                    until = wait.min(until);
                }

                // Check to see if waiting is even worth it
                if until > bundle.expiry() {
                    trace!("Bundle lifetime is shorter than wait period");
                    return self
                        .drop_bundle(
                            bundle,
                            Some(bpv7::StatusReportReasonCode::NoTimelyContactWithNextNodeOnRoute),
                        )
                        .await;
                }

                // We must wait for a bit for the CLAs to calm down
                if !self
                    .wait_to_forward(&bundle.metadata, until, &cancel_token)
                    .await?
                {
                    // Cancelled, or too long a wait for here
                    return Ok(());
                }

                // Reset retry counter, as we found a route, it's just busy
                retries = 0;
            } else if retries > self.config.max_forwarding_delay {
                if previous {
                    // We have delayed long enough trying to find a route to previous_node
                    trace!("Timed out trying to return bundle to previous node");
                    return self
                        .drop_bundle(
                            bundle,
                            Some(bpv7::StatusReportReasonCode::NoKnownRouteToDestinationFromHere),
                        )
                        .await;
                }

                trace!("Timed out trying to forward bundle");

                // Return the bundle to the source via the 'previous_node' or 'bundle.source'
                destination = bundle
                    .bundle
                    .previous_node
                    .as_ref()
                    .unwrap_or(&bundle.bundle.id.source);
                trace!("Returning bundle to previous node: {destination}");

                // Reset retry counter as we are attempting to return the bundle
                retries = 0;
                previous = true;
            } else {
                retries = retries.saturating_add(1);

                trace!("Retrying ({retries}) FIB lookup to allow FIB and CLAs to resync");

                // Async sleep for 1 second
                if !cancellable_sleep(time::Duration::seconds(1), &cancel_token).await {
                    // Cancelled
                    return Ok(());
                }
            }

            if data_is_time_sensitive {
                // Force a reload of current data, because Bundle Age may have changed
                data = None;
            }
        }
    }

    async fn wait_to_forward(
        &self,
        metadata: &metadata::Metadata,
        until: time::OffsetDateTime,
        cancel_token: &tokio_util::sync::CancellationToken,
    ) -> Result<bool, Error> {
        let wait = until - time::OffsetDateTime::now_utc();
        if wait > time::Duration::new(self.config.wait_sample_interval as i64, 0) {
            // Nothing to do now, set bundle status to Waiting, and it will be picked up later
            trace!("Bundle will wait offline until: {until}");
            self.store
                .set_status(
                    &metadata.storage_name,
                    &metadata::BundleStatus::Waiting(until),
                )
                .await?;
            return Ok(false);
        }

        // We must wait here, as we have missed the scheduled wait interval
        trace!("Waiting to forward bundle inline until: {until}");
        Ok(cancellable_sleep(wait, cancel_token).await)
    }

    fn update_extension_blocks(
        &self,
        bundle: &metadata::Bundle,
        data: &[u8],
    ) -> Result<(Vec<u8>, bool), Error> {
        let mut editor = bpv7::Editor::new(&bundle.bundle)
            // Previous Node Block
            .replace_extension_block(bpv7::BlockType::PreviousNode)
            .flags(bpv7::BlockFlags {
                must_replicate: true,
                report_on_failure: true,
                delete_bundle_on_failure: true,
                ..Default::default()
            })
            .data(cbor::encode::emit_array(Some(1), |a| {
                a.emit(
                    self.config
                        .admin_endpoints
                        .get_admin_endpoint(&bundle.bundle.destination),
                )
            }))
            .build();

        // Increment Hop Count
        if let Some(mut hop_count) = bundle.bundle.hop_count {
            editor = editor
                .replace_extension_block(bpv7::BlockType::HopCount)
                .flags(bpv7::BlockFlags {
                    must_replicate: true,
                    report_on_failure: true,
                    delete_bundle_on_failure: true,
                    ..Default::default()
                })
                .data(cbor::encode::emit_array(Some(2), |a| {
                    hop_count.count += 1;
                    a.emit(hop_count.limit);
                    a.emit(hop_count.count);
                }))
                .build();
        }

        // Update Bundle Age, if required
        let mut is_time_sensitive = false;
        if bundle.bundle.age.is_some() || bundle.bundle.id.timestamp.creation_time.is_none() {
            // We have a bundle age block already, or no valid clock at bundle source
            // So we must add an updated bundle age block
            let bundle_age = (time::OffsetDateTime::now_utc() - bundle.creation_time())
                .whole_milliseconds()
                .clamp(0, u64::MAX as i128) as u64;

            editor = editor
                .replace_extension_block(bpv7::BlockType::BundleAge)
                .flags(bpv7::BlockFlags {
                    must_replicate: true,
                    report_on_failure: true,
                    delete_bundle_on_failure: true,
                    ..Default::default()
                })
                .data(cbor::encode::emit_array(Some(1), |a| a.emit(bundle_age)))
                .build();

            // If we have a bundle age, then we are time sensitive
            is_time_sensitive = true;
        }

        editor
            .build(data)
            .map(|(_, data)| (data, is_time_sensitive))
            .map_err(|e| e.into())
    }

    #[instrument(skip(self))]
    async fn drop_bundle(
        &self,
        bundle: metadata::Bundle,
        reason: Option<bpv7::StatusReportReasonCode>,
    ) -> Result<(), Error> {
        if let Some(reason) = reason {
            self.report_bundle_deletion(&bundle, reason).await?;
        }
        trace!("Discarding bundle, leaving tombstone");
        self.store.remove(&bundle.metadata.storage_name).await
    }

    #[instrument(skip(self))]
    pub async fn report_bundle_reception(
        &self,
        bundle: &metadata::Bundle,
        reason: bpv7::StatusReportReasonCode,
    ) -> Result<(), Error> {
        // Check if a report is requested
        if !self.config.status_reports || !bundle.bundle.flags.receipt_report_requested {
            return Ok(());
        }

        trace!("Reporting bundle reception to {}", &bundle.bundle.report_to);

        self.dispatch_status_report(
            cbor::encode::emit(bpv7::AdministrativeRecord::BundleStatusReport(
                bpv7::BundleStatusReport {
                    bundle_id: bundle.bundle.id.clone(),
                    received: Some(bpv7::StatusAssertion(
                        if bundle.bundle.flags.report_status_time {
                            if let Some(t) = bundle.metadata.received_at {
                                Some(t.try_into()?)
                            } else {
                                None
                            }
                        } else {
                            None
                        },
                    )),
                    reason,
                    ..Default::default()
                },
            )),
            &bundle.bundle.report_to,
        )
        .await
    }

    #[instrument(skip(self))]
    pub async fn report_bundle_forwarded(&self, bundle: &metadata::Bundle) -> Result<(), Error> {
        // Check if a report is requested
        if !self.config.status_reports || !bundle.bundle.flags.forward_report_requested {
            return Ok(());
        }

        trace!(
            "Reporting bundle as forwarded to {}",
            &bundle.bundle.report_to
        );

        self.dispatch_status_report(
            cbor::encode::emit(bpv7::AdministrativeRecord::BundleStatusReport(
                bpv7::BundleStatusReport {
                    bundle_id: bundle.bundle.id.clone(),
                    forwarded: Some(bpv7::StatusAssertion(
                        bundle
                            .bundle
                            .flags
                            .report_status_time
                            .then(bpv7::DtnTime::now),
                    )),
                    ..Default::default()
                },
            )),
            &bundle.bundle.report_to,
        )
        .await
    }

    #[instrument(skip(self))]
    async fn report_bundle_delivery(&self, bundle: &metadata::Bundle) -> Result<(), Error> {
        // Check if a report is requested
        if !self.config.status_reports || !bundle.bundle.flags.delivery_report_requested {
            return Ok(());
        }

        trace!("Reporting bundle delivery to {}", &bundle.bundle.report_to);

        // Create a bundle report
        self.dispatch_status_report(
            cbor::encode::emit(bpv7::AdministrativeRecord::BundleStatusReport(
                bpv7::BundleStatusReport {
                    bundle_id: bundle.bundle.id.clone(),
                    delivered: Some(bpv7::StatusAssertion(
                        bundle
                            .bundle
                            .flags
                            .report_status_time
                            .then(bpv7::DtnTime::now),
                    )),
                    ..Default::default()
                },
            )),
            &bundle.bundle.report_to,
        )
        .await
    }

    #[instrument(skip(self))]
    pub async fn report_bundle_deletion(
        &self,
        bundle: &metadata::Bundle,
        reason: bpv7::StatusReportReasonCode,
    ) -> Result<(), Error> {
        // Check if a report is requested
        if !self.config.status_reports || !bundle.bundle.flags.delete_report_requested {
            return Ok(());
        }

        trace!("Reporting bundle deletion to {}", &bundle.bundle.report_to);

        // Create a bundle report
        self.dispatch_status_report(
            cbor::encode::emit(bpv7::AdministrativeRecord::BundleStatusReport(
                bpv7::BundleStatusReport {
                    bundle_id: bundle.bundle.id.clone(),
                    deleted: Some(bpv7::StatusAssertion(
                        bundle
                            .bundle
                            .flags
                            .report_status_time
                            .then(bpv7::DtnTime::now),
                    )),
                    reason,
                    ..Default::default()
                },
            )),
            &bundle.bundle.report_to,
        )
        .await
    }

    async fn dispatch_status_report(
        &self,
        payload: Vec<u8>,
        report_to: &bpv7::Eid,
    ) -> Result<(), Error> {
        // Create a bundle report
        let (bundle, data) = bpv7::Builder::new()
            .flags(bpv7::BundleFlags {
                is_admin_record: true,
                ..Default::default()
            })
            .source(&self.config.admin_endpoints.get_admin_endpoint(report_to))
            .destination(report_to)
            .add_payload_block(payload)
            .build()?;

        // Store to store
        let metadata = self
            .store
            .store(&bundle, data, metadata::BundleStatus::DispatchPending, None)
            .await?
            .trace_expect("Duplicate bundle created by builder!");

        // And queue it up
        self.enqueue_bundle(metadata::Bundle { metadata, bundle })
            .await
    }

    #[instrument(skip(self))]
    pub async fn local_dispatch(&self, mut request: SendRequest) -> Result<(), Error> {
        // Check to see if we should use ipn 2-element encoding
        if let bpv7::Eid::Ipn3 {
            allocator_id: da,
            node_number: dn,
            service_number: ds,
        } = &request.destination
        {
            // Check configured entries
            if !self
                .config
                .ipn_2_element
                .find(&request.destination)
                .is_empty()
            {
                if let bpv7::Eid::Ipn3 {
                    allocator_id: sa,
                    node_number: sn,
                    service_number: ss,
                } = &request.source
                {
                    request.source = bpv7::Eid::Ipn2 {
                        allocator_id: *sa,
                        node_number: *sn,
                        service_number: *ss,
                    };
                }
                request.destination = bpv7::Eid::Ipn2 {
                    allocator_id: *da,
                    node_number: *dn,
                    service_number: *ds,
                };
            }
        }

        // Build the bundle
        let mut b = bpv7::Builder::new()
            .source(&request.source)
            .destination(&request.destination);

        // Set flags
        if let Some(flags) = request.flags {
            b = b.flags(flags).report_to(
                &self
                    .config
                    .admin_endpoints
                    .get_admin_endpoint(&request.destination),
            );
        }

        // Lifetime
        if let Some(lifetime) = request.lifetime {
            b = b.lifetime(lifetime);
        }

        // Add payload and build
        let (bundle, data) = b.add_payload_block(request.data).build()?;

        // Store to store
        let metadata = self
            .store
            .store(&bundle, data, metadata::BundleStatus::DispatchPending, None)
            .await?
            .trace_expect("Duplicate bundle created by builder!");

        // And queue it up
        self.enqueue_bundle(metadata::Bundle { metadata, bundle })
            .await
    }

    #[instrument(skip(self))]
    async fn reassemble(
        &self,
        _bundle: metadata::Bundle,
    ) -> Result<Option<metadata::Bundle>, Error> {
        // TODO: We need to handle the case when the reassembled fragment is larger than our total RAM!

        todo!()
    }

    async fn load_data(
        &self,
        bundle: &metadata::Bundle,
    ) -> Result<Option<hardy_bpa_api::storage::DataRef>, Error> {
        // Try to load the data, but treat errors as 'Storage Depleted'
        let data = self.store.load_data(&bundle.metadata.storage_name).await?;
        if data.is_none() {
            // Report the bundle has gone
            self.report_bundle_deletion(bundle, bpv7::StatusReportReasonCode::DepletedStorage)
                .await?;
        }
        Ok(data)
    }

    #[instrument(skip(self))]
    async fn administrative_bundle(&self, bundle: metadata::Bundle) -> Result<(), Error> {
        // This is a bundle for an Admin Endpoint
        if !bundle.bundle.flags.is_admin_record {
            trace!("Received a bundle for an administrative endpoint that isn't marked as an administrative record");
            return self
                .drop_bundle(
                    bundle,
                    Some(bpv7::StatusReportReasonCode::BlockUnintelligible),
                )
                .await;
        }

        let Some(data) = self.load_data(&bundle).await? else {
            // Bundle data was deleted sometime during processing
            return Ok(());
        };

        let reason = match cbor::decode::parse::<bpv7::AdministrativeRecord>((*data).as_ref()) {
            Err(e) => {
                trace!("Failed to parse administrative record: {e}");
                Some(bpv7::StatusReportReasonCode::BlockUnintelligible)
            }
            Ok(bpv7::AdministrativeRecord::BundleStatusReport(report)) => {
                // Check if the report is for a bundle sourced from a local service
                if !self
                    .config
                    .admin_endpoints
                    .is_local_service(&report.bundle_id.source)
                {
                    trace!("Received spurious bundle status report {:?}", report);
                    Some(bpv7::StatusReportReasonCode::DestinationEndpointIDUnavailable)
                } else {
                    // Find a live service to notify
                    if let Some(endpoint) = self.app_registry.find_by_eid(&report.bundle_id.source)
                    {
                        // Notify the service
                        if let Some(assertion) = report.received {
                            endpoint
                                .status_notify(
                                    &report.bundle_id,
                                    app_registry::StatusKind::Received,
                                    report.reason,
                                    assertion.0.map(|t| t.into()),
                                )
                                .await
                        }
                        if let Some(assertion) = report.forwarded {
                            endpoint
                                .status_notify(
                                    &report.bundle_id,
                                    app_registry::StatusKind::Forwarded,
                                    report.reason,
                                    assertion.0.map(|t| t.into()),
                                )
                                .await
                        }
                        if let Some(assertion) = report.delivered {
                            endpoint
                                .status_notify(
                                    &report.bundle_id,
                                    app_registry::StatusKind::Delivered,
                                    report.reason,
                                    assertion.0.map(|t| t.into()),
                                )
                                .await
                        }
                        if let Some(assertion) = report.deleted {
                            endpoint
                                .status_notify(
                                    &report.bundle_id,
                                    app_registry::StatusKind::Deleted,
                                    report.reason,
                                    assertion.0.map(|t| t.into()),
                                )
                                .await
                        }
                    }
                    Some(bpv7::StatusReportReasonCode::NoAdditionalInformation)
                }
            }
        };

        // Done with the bundle
        self.drop_bundle(bundle, reason).await
    }

    #[instrument(skip(self))]
    pub async fn collect(
        &self,
        destination: bpv7::Eid,
        bundle_id: String,
    ) -> Result<Option<CollectResponse>, Error> {
        // Lookup bundle
        let Some(bundle) = self
            .store
            .load(&bpv7::BundleId::from_key(&bundle_id)?)
            .await?
        else {
            return Ok(None);
        };

        // Double check that we are returning something valid
        if let metadata::BundleStatus::CollectionPending = &bundle.metadata.status {
            let expiry = bundle.expiry();
            if expiry <= time::OffsetDateTime::now_utc() || bundle.bundle.destination != destination
            {
                return Ok(None);
            }

            // Get the data!
            let Some(data) = self.load_data(&bundle).await? else {
                // Bundle data was deleted sometime during processing
                return Ok(None);
            };

            // By the time we get here, we're safe to report delivery
            self.report_bundle_delivery(&bundle).await?;

            // Prepare the response
            let response = CollectResponse {
                bundle_id: bundle.bundle.id.to_key(),
                data: (*data).as_ref().to_vec(),
                expiry,
                app_ack_requested: bundle.bundle.flags.app_ack_requested,
            };

            // And we can now tombstone the bundle
            self.store
                .remove(&bundle.metadata.storage_name)
                .await
                .map(|_| Some(response))
        } else {
            Ok(None)
        }
    }

    #[instrument(skip(self))]
    pub async fn poll_for_collection(
        &self,
        destination: bpv7::Eid,
    ) -> Result<Vec<(String, time::OffsetDateTime)>, Error> {
        self.store.poll_for_collection(destination).await
    }
}
