use super::*;

impl Dispatcher {
    #[cfg_attr(feature = "instrument", instrument(skip(self,adapter,bundle),fields(bundle.id = %bundle.bundle.id)))]
    pub async fn forward_bundle(
        &self,
        adapter: &cla::adapter::Adapter,
        mut bundle: bundle::Bundle,
    ) {
        // Get bundle data from store, now we know we need it!
        let Some(data) = self.load_data(&bundle).await else {
            // Bundle data was deleted sometime during processing
            return self
                .drop_bundle(bundle, Some(ReasonCode::DepletedStorage))
                .await;
        };

        // Increment Hop Count, etc...
        // We ignore the fact that a new bundle has been created, as it makes no difference below
        let data = match self.update_extension_blocks(&bundle, &data) {
            Err(e) => {
                warn!("Failed to update extension blocks: {e}");
                self.store
                    .update_status(&mut bundle, &bundle::BundleStatus::Waiting)
                    .await;
                self.store.watch_bundle(bundle).await;
                return;
            }
            Ok(data) => data,
        };

        // - Runs after routing selects a CLA peer, just before CLA send
        // - Modifications are in-memory only (like Deliver), NOT persisted
        // - If send fails or peer goes down, bundle returns to Waiting and may
        //   route to a different peer, so Egress will run again with fresh context
        // - BPSec blocks (BIB/BCB) should be added here, may be peer-specific
        // - On Drop result: call drop_bundle() and return early
        let (mut bundle, data) = match self
            .filter_engine
            .exec(
                filter::Hook::Egress,
                bundle,
                Bytes::from(data),
                self.key_provider(),
                &self.processing_pool,
            )
            .await
        {
            Ok(filter::ExecResult::Continue(_, bundle, data)) => (bundle, data),
            Ok(filter::ExecResult::Drop(bundle, reason)) => {
                return self.drop_bundle(bundle, reason).await;
            }
            Err(e) => {
                // Bundle was moved into exec() and is consumed on error.
                // It still exists in storage with its previous status and will
                // be retried on the next poll_waiting cycle.
                warn!("Egress filter execution failed: {e}");
                return;
            }
        };

        // Build forwarding context from bundle metadata
        let next_hop = bundle
            .metadata
            .read_only
            .next_hop
            .as_ref()
            .cloned()
            .unwrap_or_else(|| bundle.bundle.destination.clone());

        let info = cla::ForwardInfo {
            next_hop: &next_hop,
            flow_label: bundle.metadata.writable.flow_label,
        };

        match adapter.cla.forward(&info, data).await {
            Ok(cla::ForwardBundleResult::Sent) => {
                metrics::counter!("bpa.bundle.forwarded").increment(1);
                self.report_bundle_forwarded(&bundle).await;

                self.report_bundle_deletion(&bundle, ReasonCode::NoAdditionalInformation)
                    .await;
                return self.delete_bundle(bundle).await;
            }
            Ok(cla::ForwardBundleResult::NoNeighbour) => {
                debug!("CLA indicates neighbour has gone");
            }
            Err(e) => {
                metrics::counter!("bpa.bundle.forwarding.failed").increment(1);
                debug!("Failed to forward bundle: {e}");
            }
        }

        // Forwarding failed — set bundle to Waiting for retry
        self.store
            .update_status(&mut bundle, &bundle::BundleStatus::Waiting)
            .await;
        self.store.watch_bundle(bundle).await;
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    fn update_extension_blocks(
        &self,
        bundle: &bundle::Bundle,
        source_data: &[u8],
    ) -> Result<Box<[u8]>, hardy_bpv7::editor::Error> {
        // Previous Node Block
        let mut editor = hardy_bpv7::editor::Editor::new(&bundle.bundle, source_data)
            .insert_block(hardy_bpv7::block::Type::PreviousNode)
            .map_err(|(_, e)| e)?
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
                .map_err(|(_, e)| e)?
                .with_flags(hardy_bpv7::block::Flags {
                    report_on_failure: true,
                    must_replicate: true,
                    ..Default::default()
                })
                .with_data(
                    hardy_cbor::encode::emit(&hardy_bpv7::hop_info::HopInfo {
                        limit: hop_count.limit,
                        count: hop_count.count.saturating_add(1),
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
                .map_err(|(_, e)| e)?
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
