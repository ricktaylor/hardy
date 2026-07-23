use super::*;

impl Dispatcher {
    #[cfg_attr(feature = "instrument", instrument(skip(self,cla,bundle),fields(bundle.id = %bundle.bundle.id)))]
    pub async fn forward_bundle(
        &self,
        cla: &dyn cla::Cla,
        peer: u32,
        queue: Option<u32>,
        cla_addr: &cla::ClaAddress,
        bundle: bundle::Bundle,
    ) {
        // Get bundle data from store, now we know we need it!
        let Some((mut bundle, data)) = self.load_data_or_drop(bundle).await else {
            return;
        };

        // Record the ownership hand-off before the in-memory rewrite below and
        // before offering the bundle: a deferred outcome can arrive on another
        // task the instant the CLA accepts, and transfer_outcome() only
        // honours bundles already in ForwardAckPending. The persist must
        // happen while the metadata still indexes the stored (un-rewritten)
        // data. This also distinguishes an in-flight transfer from a queued
        // one, so reset_peer_queue() no longer races the offer.
        self.store
            .update_status(
                &mut bundle,
                &bundle::BundleStatus::ForwardAckPending { peer },
            )
            .await;

        // Increment Hop Count, etc... The rewrite shifts block extents, and
        // the Egress filters below receive (bundle, data) as a consistent
        // pair, so the updated Bundle must replace the pre-rewrite one. The
        // pre-rewrite Bundle is kept: the rewrite is in-memory only, and a
        // bundle parked back to Waiting must persist metadata that indexes
        // the stored (un-rewritten) data.
        let (pre_rewrite, data) = match self.update_extension_blocks(&bundle, data) {
            Err(e) => {
                warn!("Failed to update extension blocks: {e}");
                self.store
                    .update_status(&mut bundle, &bundle::BundleStatus::Waiting)
                    .await;
                return self.store.watch_bundle(bundle).await;
            }
            Ok((new_bundle, data)) => (core::mem::replace(&mut bundle.bundle, new_bundle), data),
        };

        // - Runs after dequeue from ForwardPending, just before CLA send
        // - Modifications are in-memory only (like Deliver), NOT persisted
        // - If send fails or peer goes down, bundle returns to Waiting and may
        //   route to a different peer, so Egress will run again with fresh context
        // - BPSec blocks (BIB/BCB) should be added here, may be peer-specific
        // - On Drop result: call drop_bundle() and return early
        let (mut bundle, data) = match self
            .filter_engine
            .exec(filter::Hook::Egress, bundle, data, self.key_provider())
            .await
        {
            Ok(filter::ExecResult::Continue(_, bundle, data)) => (bundle, data),
            Ok(filter::ExecResult::Drop(bundle, reason)) => {
                if let Some(reason) = reason {
                    return self.drop_bundle(bundle, reason).await;
                } else {
                    return self.delete_bundle(bundle).await;
                }
            }
            Err(e) => {
                error!("Egress filter execution failed: {e}");
                return;
            }
        };

        // And pass to CLA
        match cla.forward(queue, cla_addr, &bundle.bundle.id, data).await {
            Ok(cla::ForwardBundleResult::Sent) => {
                metrics::counter!("bpa.bundle.forwarded").increment(1);
                self.report_bundle_forwarded(&bundle).await;

                // Don't use drop_bundle() as we do not want to count the Drop as a 'dropped bundle'
                self.report_bundle_deletion(&bundle, ReasonCode::NoAdditionalInformation)
                    .await;
                return self.delete_bundle(bundle).await;
            }
            Ok(cla::ForwardBundleResult::Accepted) => {
                // The CLA owns the transfer; the bundle stays in
                // ForwardAckPending until the outcome arrives, the peer is
                // removed, or the bundle's lifetime expires.
                return self.store.watch_bundle(bundle).await;
            }
            Ok(cla::ForwardBundleResult::NoNeighbour) => {
                // The neighbour has gone, kill the queue
                debug!(
                    "CLA indicates neighbour has gone, clearing queue assignment for peer {peer}"
                );
            }
            Err(e) => {
                metrics::counter!("bpa.bundle.forwarding.failed").increment(1);
                debug!("Failed to forward bundle to peer {peer}: {e}, clearing queue assignment");
            }
        }

        // Synchronous failure: this bundle never entered the channel. Restore
        // the pre-rewrite Bundle (the stored data is the un-rewritten
        // original) and return it to Waiting for a fresh routing decision
        // along with the rest of the peer's queue.
        bundle.bundle = pre_rewrite;
        self.store
            .update_status(&mut bundle, &bundle::BundleStatus::Waiting)
            .await;
        self.store.watch_bundle(bundle).await;
        self.store.reset_peer_queue(peer).await;
    }

    // Resolves a deferred transfer outcome reported by `cla` for a bundle it
    // previously answered `Accepted`. The status check is the stale-outcome
    // guard: anything not currently ForwardAckPending via a peer of the
    // reporting CLA — already resolved, expired, another CLA's transfer — is
    // logged and dropped.
    #[cfg_attr(feature = "instrument", instrument(skip(self, cla), fields(bundle.id = %bundle_id)))]
    pub async fn transfer_outcome(
        &self,
        cla: &cla::registry::Cla,
        bundle_id: &hardy_bpv7::bundle::Id,
        outcome: cla::TransferOutcome,
    ) {
        let Some(mut bundle) = self.store.get_metadata(bundle_id).await else {
            debug!("Transfer outcome for unknown bundle {bundle_id}, ignored");
            return;
        };

        let bundle::BundleStatus::ForwardAckPending { peer } = bundle.metadata.status else {
            debug!(
                "Transfer outcome for bundle {bundle_id} that is not awaiting one ({:?}), ignored",
                bundle.metadata.status
            );
            return;
        };

        if !cla.owns_peer(peer) {
            warn!("Transfer outcome for peer {peer} from a CLA that does not own it, ignored");
            return;
        }

        match outcome {
            cla::TransferOutcome::Delivered => {
                metrics::counter!("bpa.bundle.forwarded").increment(1);
                self.report_bundle_forwarded(&bundle).await;

                // Don't use drop_bundle() as we do not want to count the Drop as a 'dropped bundle'
                self.report_bundle_deletion(&bundle, ReasonCode::NoAdditionalInformation)
                    .await;
                self.delete_bundle(bundle).await
            }
            cla::TransferOutcome::Failed => {
                metrics::counter!("bpa.bundle.forwarding.failed").increment(1);

                // Bundle-scoped evidence about a single transfer: re-run the
                // routing decision now, rather than parking in Waiting (whose
                // semantic is "nowhere to go") or resetting the whole peer
                // queue (link-scoped evidence). Dispatch parks the bundle in
                // Waiting itself if no route remains.
                self.store
                    .update_status(&mut bundle, &bundle::BundleStatus::Dispatching)
                    .await;
                self.dispatch_bundle(bundle).await
            }
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    fn update_extension_blocks(
        &self,
        bundle: &bundle::Bundle,
        source_data: Bytes,
    ) -> Result<(hardy_bpv7::bundle::Bundle, Bytes), hardy_bpv7::editor::Error> {
        // Previous Node Block
        let mut editor = hardy_bpv7::editor::Editor::new(&bundle.bundle, &source_data)
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

        // rebuild_bundle() returns a Bundle whose block extents index the
        // rewritten data, keeping the (bundle, data) pair consistent for the
        // Egress filter chain
        let (new_bundle, chunks) = editor.rebuild_bundle()?;

        // Try to modify the source buffer in place if exclusively owned
        let data = match source_data.try_into_mut() {
            Ok(buf) => {
                let mut vec = buf.into();
                hardy_bpv7::editor::Chunk::flatten_inplace(chunks, &mut vec);
                Bytes::from(vec)
            }
            Err(source_data) => {
                Bytes::from(hardy_bpv7::editor::Chunk::flatten(chunks, &source_data))
            }
        };
        Ok((new_bundle, data))
    }
}
