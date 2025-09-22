use super::*;

// ForwardResult::Forwarded => {
//                 self.report_bundle_forwarded(&bundle).await;
//                 None
//             }

fn lifted_from_dispatcher() {
    if let Some(clas) = clas {
        // Get bundle data from store, now we know we need it!
        let Some(data) = self.load_data(bundle).await? else {
            // Bundle data was deleted sometime during processing
            return Ok(ForwardResult::Drop(Some(ReasonCode::DepletedStorage)));
        };

        // Track fragmentation status
        let mut max_bundle_size = None;

        // For each CLA
        for (cla, cla_addr) in clas {
            // Increment Hop Count, etc...
            // We ignore the fact that a new bundle has been created, as it makes no difference below
            let (_, data) = self.update_extension_blocks(bundle, &data);

            match cla.cla.forward(cla_addr, data.into()).await {
                Err(e) => warn!("CLA failed to forward: {e}"),
                Ok(cla::ForwardBundleResult::Sent) => {
                    // We have successfully forwarded!
                    return Ok(ForwardResult::Forwarded);
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

    // By the time we get here, we have tried every CLA
    trace!("Failed to forward bundle to {next_hop}, no route to node");

    if previous {
        trace!("Failed to return bundle to previous node, no route to node");

        return Ok(ForwardResult::Drop(Some(
            hardy_bpv7::status_report::ReasonCode::NoKnownRouteToDestinationFromHere,
        )));
    }
}

#[cfg_attr(feature = "tracing", instrument(skip_all))]
fn update_extension_blocks(
    &self,
    bundle: &bundle::Bundle,
    source_data: &[u8],
) -> (hardy_bpv7::bundle::Bundle, Box<[u8]>) {
    let mut editor = hardy_bpv7::editor::Editor::new(&bundle.bundle, source_data);

    // Previous Node Block
    editor
        .replace_block(hardy_bpv7::block::Type::PreviousNode)
        .build(hardy_cbor::encode::emit(
            &self.node_ids.get_admin_endpoint(&bundle.bundle.destination),
        ));

    // Increment Hop Count
    if let Some(hop_count) = &bundle.bundle.hop_count {
        editor
            .replace_block(hardy_bpv7::block::Type::HopCount)
            .build(hardy_cbor::encode::emit(&hardy_bpv7::hop_info::HopInfo {
                limit: hop_count.limit,
                count: hop_count.count + 1,
            }));
    }

    // Update Bundle Age, if required
    if bundle.bundle.age.is_some() || bundle.bundle.id.timestamp.creation_time.is_none() {
        // We have a bundle age block already, or no valid clock at bundle source
        // So we must add an updated bundle age block
        let bundle_age = (time::OffsetDateTime::now_utc() - bundle.creation_time())
            .whole_milliseconds()
            .clamp(0, u64::MAX as i128) as u64;

        editor
            .replace_block(hardy_bpv7::block::Type::BundleAge)
            .build(hardy_cbor::encode::emit(&bundle_age));
    }

    editor.rebuild()
}
