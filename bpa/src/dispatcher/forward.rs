use super::*;

impl Dispatcher {
    #[cfg_attr(feature = "tracing", instrument(skip(self,cla,bundle),fields(bundle.id = %bundle.bundle.id)))]
    pub async fn forward_bundle(
        self: &Arc<Self>,
        cla: &dyn cla::Cla,
        peer: u32,
        queue: Option<u32>,
        cla_addr: &cla::ClaAddress,
        bundle: bundle::Bundle,
    ) {
        // Get bundle data from store, now we know we need it!
        let Some(data) = self.load_data(&bundle).await else {
            // Bundle data was deleted sometime during processing
            return;
        };

        // Increment Hop Count, etc...
        // We ignore the fact that a new bundle has been created, as it makes no difference below
        let (_, data) = match self.update_extension_blocks(&bundle, &data) {
            Err(e) => {
                error!("Failed to update extension blocks: {e}");
                return;
            }
            Ok((bundle, data)) => (bundle, data),
        };

        // And pass to CLA
        match cla.forward(queue, cla_addr, data.into()).await {
            Ok(cla::ForwardBundleResult::Sent) => {
                self.report_bundle_forwarded(&bundle).await;
                self.drop_bundle(bundle, None).await;
                return;
            }
            Ok(cla::ForwardBundleResult::NoNeighbour) => {
                // The neighbour has gone, kill the queue
                debug!(
                    "CLA indicates neighbour has gone, clearing queue assignment for peer {}",
                    peer
                );
            }
            Err(e) => {
                error!("Failed to forward bundle: {e}");
                debug!("Clearing queue assignment for peer {}", peer);
            }
        }

        self.store.reset_peer_queue(peer).await;
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    fn update_extension_blocks(
        &self,
        bundle: &bundle::Bundle,
        source_data: &[u8],
    ) -> Result<(hardy_bpv7::bundle::Bundle, Box<[u8]>), hardy_bpv7::editor::Error> {
        // Previous Node Block
        let mut editor = hardy_bpv7::editor::Editor::new(&bundle.bundle, source_data)
            .insert_block(hardy_bpv7::block::Type::PreviousNode)
            .trace_expect("Failed to insert PreviousNode block")
            .with_flags(hardy_bpv7::block::Flags {
                report_on_failure: true,
                ..Default::default()
            })
            .with_data(
                hardy_cbor::encode::emit(
                    &self.node_ids.get_admin_endpoint(&bundle.bundle.destination),
                )
                .0
                .into(),
            )
            .rebuild();

        // Increment Hop Count
        if let Some(hop_count) = &bundle.bundle.hop_count {
            editor = editor
                .insert_block(hardy_bpv7::block::Type::HopCount)
                .trace_expect("Failed to insert HopCount block")
                .with_flags(hardy_bpv7::block::Flags {
                    report_on_failure: true,
                    must_replicate: true,
                    ..Default::default()
                })
                .with_data(
                    hardy_cbor::encode::emit(&hardy_bpv7::hop_info::HopInfo {
                        limit: hop_count.limit,
                        count: hop_count.count + 1,
                    })
                    .0
                    .into(),
                )
                .rebuild();
        }

        // Update Bundle Age, if required
        if bundle.bundle.age.is_some() || !bundle.bundle.id.timestamp.is_clocked() {
            // We have a bundle age block already, or no valid clock at bundle source
            // So we must add an updated bundle age block
            let bundle_age = (time::OffsetDateTime::now_utc() - bundle.creation_time())
                .whole_milliseconds()
                .clamp(0, u64::MAX as i128) as u64;

            editor = editor
                .insert_block(hardy_bpv7::block::Type::BundleAge)
                .trace_expect("Failed to insert BundleAge block")
                .with_flags(hardy_bpv7::block::Flags {
                    report_on_failure: true,
                    must_replicate: true,
                    ..Default::default()
                })
                .with_data(hardy_cbor::encode::emit(&bundle_age).0.into())
                .rebuild();
        }

        editor.rebuild()
    }
}
