use super::*;

impl Dispatcher {
    pub(super) async fn dispatch_bundle(&self, mut bundle: bundle::Bundle) -> Result<(), Error> {
        // Drop Eid::Null silently to cull spam
        if bundle.bundle.destination == bpv7::Eid::Null {
            return self.drop_bundle(bundle, None).await;
        }

        let mut next_hop = bundle.bundle.destination.clone();
        let mut previous = false;
        loop {
            // Perform RIB lookup
            let (clas, until) = match self.rib.find(&next_hop).await {
                Err(reason) => {
                    trace!("Bundle is black-holed");
                    return self.drop_bundle(bundle, reason).await;
                }
                Ok(Some(rib::FindResult::AdminEndpoint)) => {
                    if bundle.bundle.id.fragment_info.is_some() {
                        return self.reassemble(bundle).await;
                    }

                    // The bundle is for the Administrative Endpoint
                    return self.administrative_bundle(bundle).await;
                }
                Ok(Some(rib::FindResult::Deliver(service))) => {
                    if bundle.bundle.id.fragment_info.is_some() {
                        return self.reassemble(bundle).await;
                    }

                    // Bundle is for a local service
                    return self.deliver_bundle(service, bundle).await;
                }
                Ok(Some(rib::FindResult::Forward(clas, until))) => (clas, until),
                Ok(None) => (Vec::new(), None),
            };

            if !clas.is_empty() {
                // Get bundle data from store, now we know we need it!
                let Some(data) = self.load_data(&mut bundle).await? else {
                    // Bundle data was deleted sometime during processing
                    return Ok(());
                };

                // TODO: Pluggable Egress filters!

                // Track fragmentation status
                let mut max_bundle_size = None;

                // For each CLA
                for (cla, cla_addr) in clas {
                    // Increment Hop Count, etc...
                    let data = self.update_extension_blocks(&bundle, &data);

                    match cla.cla.on_forward(cla_addr, data.into()).await {
                        Err(e) => warn!("CLA failed to forward: {e}"),
                        Ok(cla::ForwardBundleResult::Sent) => {
                            // We have successfully forwarded!
                            self.report_bundle_forwarded(&bundle).await?;

                            // TODO: Should we drop now?  This is where Custody Transfer comes in

                            return self.drop_bundle(bundle, None).await;
                        }
                        Ok(cla::ForwardBundleResult::NoNeighbour) => {
                            trace!("CLA has no neighbour for {next_hop}");
                        }
                        Ok(cla::ForwardBundleResult::TooBig(mbs)) => {
                            // Need to fragment to fit, track the largest MTU possible to minimize number of fragments
                            max_bundle_size = max_bundle_size.max(Some(mbs));
                        }
                    }
                }

                if let Some(max_bundle_size) = max_bundle_size {
                    // Fragmentation required
                    return self.fragment(max_bundle_size, bundle).await;
                }
            }

            // See if we should wait
            if let Some(until) = until {
                return self.bundle_wait(next_hop, bundle, until).await;
            }

            // By the time we get here, we have tried every CLA
            trace!("Failed to forward bundle to {next_hop}, no route to node");

            if previous {
                trace!("Failed to return bundle to previous node, no route to node");

                return self
                    .drop_bundle(
                        bundle,
                        Some(bpv7::StatusReportReasonCode::NoKnownRouteToDestinationFromHere),
                    )
                    .await;
            }

            // Return the bundle to the source via the 'previous_node' or 'bundle.source'
            previous = true;
            next_hop = bundle
                .bundle
                .previous_node
                .as_ref()
                .unwrap_or(&bundle.bundle.id.source)
                .clone();

            trace!("Returning bundle to previous node {next_hop}");
        }
    }

    fn update_extension_blocks(&self, bundle: &bundle::Bundle, source_data: &[u8]) -> Vec<u8> {
        let mut editor = bpv7::Editor::new(&bundle.bundle, source_data);

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
                &self.node_ids.get_admin_endpoint(&bundle.bundle.destination),
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
