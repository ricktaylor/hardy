use super::*;

impl Dispatcher {
    pub(super) async fn forward_bundle(&self, mut bundle: metadata::Bundle) -> Result<(), Error> {
        if let Some(reason) = self.forward_inner(&mut bundle).await? {
            self.drop_bundle(bundle, reason).await
        } else {
            Ok(())
        }
    }

    async fn forward_inner(
        &self,
        bundle: &mut metadata::Bundle,
    ) -> Result<Option<Option<bpv7::StatusReportReasonCode>>, Error> {
        let Some(fib) = &self.fib else {
            /* If forwarding is disabled in the configuration, then we can only deliver bundles.
             * As we have decided that the bundle is not for a local service, we cannot deliver.
             * Therefore, we respond with a Destination endpoint ID unavailable report */
            trace!("Bundle should be forwarded, but forwarding is disabled");
            return Ok(Some(Some(
                bpv7::StatusReportReasonCode::DestinationEndpointIDUnavailable,
            )));
        };

        // TODO: Pluggable Egress filters!

        /* We loop here, as the FIB could tell us that there should be a CLA to use to forward
         * But it might be rebooting or jammed, so we keep retrying for a "reasonable" amount of time */
        let mut data = None;
        let mut previous = false;
        let mut retries = 0;
        let mut destination = bundle.bundle.destination.clone();

        loop {
            // Check bundle expiry
            if bundle.has_expired() {
                trace!("Bundle lifetime has expired");
                return Ok(Some(Some(bpv7::StatusReportReasonCode::LifetimeExpired)));
            }

            // Lookup/Perform actions
            let action = match fib.find(&destination).await {
                Err(reason) => {
                    trace!("Bundle is black-holed");
                    return Ok(Some(reason));
                }
                Ok(fib::ForwardAction {
                    clas,
                    wait: Some(wait),
                }) if clas.is_empty() => {
                    // Check to see if waiting is even worth it
                    if wait > bundle.expiry() {
                        trace!("Bundle lifetime is shorter than wait period");
                        return Ok(Some(Some(
                            bpv7::StatusReportReasonCode::NoTimelyContactWithNextNodeOnRoute,
                        )));
                    }

                    // Wait a bit
                    if !self.wait_to_forward(bundle, wait).await? {
                        // Cancelled, or too long a wait for here
                        return Ok(None);
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
            for endpoint in &action.clas {
                // Find the named CLA
                if let Some(e) = self.cla_registry.find(endpoint.handle).await {
                    // Get bundle data from store, now we know we need it!
                    if data.is_none() {
                        let Some(source_data) = self.load_data(&bundle).await? else {
                            // Bundle data was deleted sometime during processing
                            return Ok(None);
                        };

                        // Increment Hop Count, etc...
                        (data, data_is_time_sensitive) = self
                            .update_extension_blocks(&bundle, (*source_data).as_ref())
                            .map(|(data, data_is_time_sensitive)| {
                                (Some(data), data_is_time_sensitive)
                            })?;
                    }

                    match e.forward_bundle(&destination, data.clone().unwrap()).await {
                        Ok(cla_registry::ForwardBundleResult::Sent) => {
                            // We have successfully forwarded!
                            return self
                                .report_bundle_forwarded(&bundle)
                                .await
                                .map(|_| Some(None));
                        }
                        Ok(cla_registry::ForwardBundleResult::Pending(handle, until)) => {
                            // CLA will report successful forwarding
                            // Don't wait longer than expiry
                            let until = until.unwrap_or_else(|| {
                                warn!("CLA endpoint has not provided a suitable AckPending delay, defaulting to 1 minute");
                                time::OffsetDateTime::now_utc() + time::Duration::minutes(1)
                            }).min(bundle.expiry());

                            // Set the bundle status to 'Forward Acknowledgement Pending'
                            return self
                                .store
                                .set_status(
                                    bundle,
                                    metadata::BundleStatus::ForwardAckPending(handle, until),
                                )
                                .await
                                .map(|_| None);
                        }
                        Ok(cla_registry::ForwardBundleResult::Congested(until)) => {
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
                    return Ok(Some(Some(
                        bpv7::StatusReportReasonCode::NoTimelyContactWithNextNodeOnRoute,
                    )));
                }

                // We must wait for a bit for the CLAs to calm down
                if !self.wait_to_forward(bundle, until).await? {
                    // Cancelled, or too long a wait for here
                    return Ok(None);
                }

                // Reset retry counter, as we found a route, it's just busy
                retries = 0;
            } else if retries >= self.config.max_forwarding_delay {
                if previous {
                    // We have delayed long enough trying to find a route to previous_node
                    trace!("Failed to return bundle to previous node, no route");
                    return Ok(Some(Some(
                        bpv7::StatusReportReasonCode::NoKnownRouteToDestinationFromHere,
                    )));
                }

                trace!("Failed to forward bundle, no route");

                // Return the bundle to the source via the 'previous_node' or 'bundle.source'
                destination = bundle
                    .bundle
                    .previous_node
                    .as_ref()
                    .unwrap_or(&bundle.bundle.id.source)
                    .clone();
                trace!("Returning bundle to previous node: {destination}");

                // Reset retry counter as we are attempting to return the bundle
                retries = 0;
                previous = true;
            } else {
                retries = retries.saturating_add(1);

                trace!("Retrying ({retries}) FIB lookup to allow FIB and CLAs to resync");

                // Async sleep for 1 second
                if !cancellable_sleep(time::Duration::seconds(1), &self.cancel_token).await {
                    // Cancelled
                    return Ok(None);
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
        bundle: &mut metadata::Bundle,
        until: time::OffsetDateTime,
    ) -> Result<bool, Error> {
        let wait = until - time::OffsetDateTime::now_utc();
        if wait > time::Duration::new(self.config.wait_sample_interval as i64, 0) {
            // Nothing to do now, set bundle status to Waiting, and it will be picked up later
            trace!("Bundle will wait offline until: {until}");
            self.store
                .set_status(bundle, metadata::BundleStatus::Waiting(until))
                .await
                .map(|_| false)
        } else {
            // We must wait here, as we have missed the scheduled wait interval
            trace!("Waiting to forward bundle inline until: {until}");
            Ok(cancellable_sleep(wait, &self.cancel_token).await)
        }
    }

    fn update_extension_blocks(
        &self,
        bundle: &metadata::Bundle,
        data: &[u8],
    ) -> Result<(Box<[u8]>, bool), Error> {
        let mut editor = bpv7::Editor::new(&bundle.bundle);

        // Remove unrecognized blocks we are supposed to
        for (block_number, block) in &bundle.bundle.blocks {
            if let bpv7::BlockType::Private(_) = &block.block_type {
                if block.flags.delete_block_on_failure {
                    editor = editor.remove_extension_block(*block_number);
                }
            }
        }

        // Previous Node Block
        editor = editor
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
            .map_err(Into::into)
    }

    #[instrument(skip(self))]
    pub async fn confirm_forwarding(
        &self,
        handle: u32,
        bundle_id: &str,
    ) -> Result<(), tonic::Status> {
        let Some(bundle) = self
            .store
            .load(
                &bpv7::BundleId::from_key(bundle_id)
                    .map_err(|e| tonic::Status::from_error(e.into()))?,
            )
            .await
            .map_err(tonic::Status::from_error)?
        else {
            return Err(tonic::Status::not_found("No such bundle"));
        };

        match &bundle.metadata.status {
            metadata::BundleStatus::ForwardAckPending(t, _) if t == &handle => {
                // Report bundle forwarded
                self.report_bundle_forwarded(&bundle)
                    .await
                    .map_err(tonic::Status::from_error)?;

                // And drop the bundle
                self.drop_bundle(bundle, None)
                    .await
                    .map_err(tonic::Status::from_error)
            }
            _ => Err(tonic::Status::not_found("No such bundle")),
        }
    }
}
