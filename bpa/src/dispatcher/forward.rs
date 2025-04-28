use super::*;

impl Dispatcher {
    pub(super) async fn forward_bundle(
        &self,
        bundle: &mut bundle::Bundle,
    ) -> Result<DispatchResult, Error> {
        // TODO: Pluggable Egress filters!

        let mut destination = &bundle.bundle.destination;
        let mut previous = false;
        loop {
            // Check bundle expiry
            if bundle.has_expired() {
                trace!("Bundle lifetime has expired");
                return Ok(DispatchResult::Drop(Some(
                    bpv7::StatusReportReasonCode::LifetimeExpired,
                )));
            }

            // Lookup CLAs to forward
            let result = match self.rib.find(destination).await {
                Err(reason) => {
                    trace!("Bundle is black-holed");
                    return Ok(DispatchResult::Drop(reason));
                }
                Ok(result) => result,
            };

            if !result.clas.is_empty() {
                // Get bundle data from store, now we know we need it!
                let Some(data) = self.load_data(bundle).await? else {
                    // Bundle data was deleted sometime during processing
                    return Ok(DispatchResult::Done);
                };

                // Increment Hop Count, etc...
                let data = self.update_extension_blocks(bundle, data);

                // Track fragmentation status
                let mut fragment_mtu = None;

                // For each CLA
                for cla in result.clas {
                    match self
                        .cla_registry
                        .forward(&cla, destination, &data)
                        .await
                        .inspect_err(|e| error!("CLA failed to forward: {e}"))?
                    {
                        cla::ForwardBundleResult::Sent => {
                            // We have successfully forwarded!
                            return self
                                .report_bundle_forwarded(bundle)
                                .await
                                .map(|_| DispatchResult::Drop(None));
                        }
                        cla::ForwardBundleResult::NoNeighbour => {
                            trace!("CLA has no neighbour for {destination}");
                        }
                        cla::ForwardBundleResult::TooBig(mtu) => {
                            // Need to fragment to fit, track the largest MTU possible to minimize number of fragments
                            if let Some(f_mtu) = fragment_mtu {
                                if mtu > f_mtu {
                                    fragment_mtu = Some(mtu);
                                }
                            } else {
                                fragment_mtu = Some(mtu);
                            }
                        }
                    }
                }

                if let Some(mtu) = fragment_mtu {
                    // Fragmentation required
                    return self.fragment(mtu, bundle, data).await;
                }
            }

            // By the time we get here, we have tried every CLA
            trace!("Failed to forward bundle to {destination}, no route to node");

            // See if we should wait
            if let Some(until) = result.until {
                match self.bundle_wait(destination, bundle, until).await {
                    DispatchResult::Continue => {}
                    r => return Ok(r),
                }
            } else if previous {
                trace!("Failed to return bundle to previous node, no route to node");

                return Ok(DispatchResult::Drop(Some(
                    bpv7::StatusReportReasonCode::NoKnownRouteToDestinationFromHere,
                )));
            } else {
                // Return the bundle to the source via the 'previous_node' or 'bundle.source'
                previous = true;
                destination = bundle
                    .bundle
                    .previous_node
                    .as_ref()
                    .unwrap_or(&bundle.bundle.id.source);

                trace!("Returning bundle to previous node {destination}");
            }
        }
    }

    async fn bundle_wait(
        &self,
        destination: &bpv7::Eid,
        bundle: &bundle::Bundle,
        until: time::OffsetDateTime,
    ) -> DispatchResult {
        // Check to see if waiting is even worth it
        if until > bundle.expiry() {
            trace!(
                "Bundle lifetime {} is less than wait deadline {until}",
                bundle.expiry()
            );
            return DispatchResult::Drop(Some(
                bpv7::StatusReportReasonCode::NoTimelyContactWithNextNodeOnRoute,
            ));
        }

        // Check to see if we should wait at all!
        let duration = until - time::OffsetDateTime::now_utc();
        if !duration.is_positive() {
            return DispatchResult::Drop(Some(
                bpv7::StatusReportReasonCode::NoTimelyContactWithNextNodeOnRoute,
            ));
        }

        trace!("Waiting to forward until {until}");

        self.rib
            .wait_for_route(
                destination,
                tokio::time::Duration::new(
                    duration.whole_seconds() as u64,
                    duration.subsec_nanoseconds() as u32,
                ),
                &self.cancel_token,
            )
            .await;

        if self.cancel_token.is_cancelled() {
            DispatchResult::Done
        } else {
            DispatchResult::Continue
        }
    }

    fn update_extension_blocks(
        &self,
        bundle: &bundle::Bundle,
        source_data: storage::DataRef,
    ) -> Vec<u8> {
        let mut editor = bpv7::Editor::new(&bundle.bundle, source_data.as_ref().as_ref());

        // Remove unrecognized blocks we are supposed to
        for (block_number, block) in &bundle.bundle.blocks {
            if let bpv7::BlockType::Unrecognised(_) = &block.block_type {
                if block.flags.delete_block_on_failure {
                    editor.remove_extension_block(*block_number);
                }
            }
        }

        // Previous Node Block
        editor
            .replace_extension_block(bpv7::BlockType::PreviousNode)
            .data(cbor::encode::emit(
                self.admin_endpoints
                    .get_admin_endpoint(&bundle.bundle.destination),
            ))
            .build();

        // Increment Hop Count
        if let Some(hop_count) = &bundle.bundle.hop_count {
            editor
                .replace_extension_block(bpv7::BlockType::HopCount)
                .data(cbor::encode::emit(&bpv7::HopInfo {
                    limit: hop_count.limit,
                    count: hop_count.count + 1,
                }))
                .build();
        }

        // Update Bundle Age, if required
        if bundle.bundle.age.is_some() || bundle.bundle.id.timestamp.creation_time.is_none() {
            // We have a bundle age block already, or no valid clock at bundle source
            // So we must add an updated bundle age block
            let bundle_age = (time::OffsetDateTime::now_utc() - bundle.creation_time())
                .whole_milliseconds()
                .clamp(0, u64::MAX as i128) as u64;

            editor
                .replace_extension_block(bpv7::BlockType::BundleAge)
                .data(cbor::encode::emit(bundle_age))
                .build();
        }

        editor.build()
    }
}
