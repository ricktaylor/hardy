use super::*;

impl Dispatcher {
    #[cfg_attr(feature = "instrument", instrument(skip(self,cla,bundle),fields(bundle.id = %bundle.id())))]
    pub async fn forward_bundle(
        &self,
        cla: &dyn cla::Cla,
        peer: u32,
        lane: Option<u32>,
        cla_addr: &cla::ClaAddress,
        bundle: bundle::Bundle,
    ) {
        // The queue-assignment record carries the resolved adjacency, and
        // the claim below overwrites the status — take it first. The egress
        // channel only delivers this queue's assignments, so any other
        // status here is a stale copy whose owner resolves it elsewhere.
        let bundle::BundleStatus::ForwardPending { next_hop, .. } = &bundle.status else {
            debug!("Bundle reached forwarding without a queue assignment, dropping copy");
            return;
        };
        let next_hop = next_hop.clone();

        // Get bundle data from store, now we know we need it!
        let Some((mut bundle, data)) = self.load_data_or_drop(bundle).await else {
            return;
        };

        // Snapshot the routing table before the claim: the parks below
        // re-check it to close the park-vs-poll window (see park_bundle).
        let seen = self.rib.table_snapshot();

        // Claim the bundle out of its peer queue before the in-memory rewrite
        // below and before offering it. The claim must be a conditional swap:
        // the egress channel delivers at-least-once, so a duplicate copy
        // recovered by the storage poller must lose here rather than produce
        // a second offer. It must happen first: a deferred outcome can arrive
        // on another task the instant the CLA accepts, and transfer_outcome()
        // only honours bundles already in ForwardAckPending, while the
        // persist needs the metadata still indexing the stored (un-rewritten)
        // data. The new status also distinguishes an in-flight transfer from
        // a queued one, so reset_peer_queue() no longer races the offer.
        if !self
            .store
            .swap_status(
                &mut bundle,
                &bundle::BundleStatus::ForwardAckPending { peer },
            )
            .await
        {
            debug!("Bundle already claimed for forwarding or swept, skipping offer");
            return;
        }

        // Increment Hop Count, etc... The rewrite shifts block extents, and
        // the Egress filters below receive (bundle, data) as a consistent
        // pair, so the rebuilt block map must replace the pre-rewrite one.
        // The rewrite is in-memory only: parks persist status alone, and a
        // re-dispatch re-enters from the persisted representation (see
        // park_bundle), so no failure exit needs to restore the pre-rewrite
        // map.
        let data = match self.update_extension_blocks(&bundle, data, &next_hop) {
            Err(e) => {
                warn!("Failed to update extension blocks: {e}");
                return self
                    .park_bundle(bundle, bundle::BundleStatus::Waiting, &seen)
                    .await;
            }
            Ok((new_bundle, data)) => {
                bundle.bpv7.blocks = new_bundle.blocks;
                data
            }
        };

        // Egress chain: registered Rewriters extend the fixed rewrite above,
        // then Verifiers gate the final pre-BPSec wire form.
        // - Runs after dequeue from ForwardPending, just before CLA send
        // - Edits are in-memory only (like Deliver), NOT persisted
        // - If send fails or peer goes down, bundle returns to Waiting and may
        //   route to a different peer, so Egress runs again with fresh context
        // - BPSec blocks (BIB/BCB) should be added here, may be peer-specific
        //
        // Every exit below this point must resolve the claim taken above:
        // ForwardAckPending has no storage poller and the reaper defers its
        // expiry, so a bundle left there is invisible until the outcome
        // arrives, the peer is removed, or the BPA restarts.
        let (bundle, mut data) =
            match self
                .filters
                .run_egress(bundle, data, &next_hop, &*self.key_provider)
            {
                Ok(filter::ChainOutcome::Continue(bundle, data)) => (bundle, data),
                Ok(filter::ChainOutcome::Drop(bundle, Some(reason))) => {
                    return self.drop_bundle(bundle, reason).await;
                }
                Ok(filter::ChainOutcome::Drop(bundle, None)) => {
                    return self.delete_bundle(bundle).await;
                }
                Err((bundle, e)) => {
                    error!("Egress filter chain failed: {e}");

                    // The chain hands the claimed bundle back: return the claim
                    // to Waiting for a fresh routing decision, CAS-clean. Losing
                    // the park means a sweep or the reaper resolved it first.
                    return self
                        .park_bundle(bundle, bundle::BundleStatus::Waiting, &seen)
                        .await;
                }
            };

        // And pass to CLA: the whole bundle is in hand, so it travels as a
        // single Final segment.
        let total_len = data.len() as u64;
        match cla
            .forward(lane, cla_addr, bundle.id(), total_len, &mut data)
            .await
        {
            Ok(cla::ForwardBundleResult::Sent) => {
                // The terminal claim is a conditional tombstone: the reaper
                // defers in-flight transfers, but a peer sweep can still
                // resolve the bundle mid-transmit, and losing the claim means
                // its resolution has gone out. The forwarded report is
                // suppressed with the rest: a lost resolution never happened.
                if !self.store.tombstone_if(&bundle).await {
                    debug!(
                        "Forward completion for {} lost the resolution race, ignored",
                        bundle.id()
                    );
                    return;
                }
                metrics::counter!("bpa.bundle.forwarded").increment(1);
                self.report_bundle_forwarded(&bundle).await;

                // Don't use drop_bundle() as we do not want to count the Drop as a 'dropped bundle'
                self.report_bundle_deletion(&bundle, ReasonCode::NoAdditionalInformation)
                    .await;
                return self.delete_bundle(bundle).await;
            }
            Ok(cla::ForwardBundleResult::Accepted) => {
                // The CLA owns the transfer; the bundle stays in
                // ForwardAckPending until the outcome arrives or the peer is
                // removed. The watch stays armed even though the expiry pass
                // defers this status: if a peer sweep parks the bundle before
                // its expiry, the live entry still reaps it promptly.
                return self.store.watch_bundle(bundle).await;
            }
            Ok(cla::ForwardBundleResult::NoNeighbour) => {
                // Link-scoped evidence: the neighbour is gone. Return the
                // bundle to Waiting, and reset the whole peer queue so its
                // bundles await a fresh routing decision alongside it.
                debug!(
                    "CLA indicates neighbour has gone, clearing queue assignment for peer {peer}"
                );
                self.park_bundle(bundle, bundle::BundleStatus::Waiting, &seen)
                    .await;
                self.store.reset_peer_queue(peer).await;
            }
            Err(e) => {
                metrics::counter!("bpa.bundle.forwarding.failed").increment(1);
                debug!("Failed to forward bundle to peer {peer}: {e}, returning it to Waiting");

                // Bundle-scoped evidence about a single transfer: park only
                // this bundle, leaving the rest of the peer's queue alone —
                // resetting the queue is the response to link-scoped
                // evidence, above. Unlike the deferred `Failed` outcome,
                // which is paced by a network round trip, a synchronous
                // failure can be deterministic and instantaneous, so
                // re-running dispatch inline here could spin; the retry
                // waits in Waiting for the next routing or link event —
                // park_bundle re-dispatches at most once, and only if such
                // an event landed while this transfer was in flight.
                self.park_bundle(bundle, bundle::BundleStatus::Waiting, &seen)
                    .await;
            }
        }
    }

    // Resolves a deferred transfer outcome reported by `cla` for a bundle it
    // previously answered `Accepted`. The status check is the stale-outcome
    // guard: anything not currently ForwardAckPending via a peer of the
    // reporting CLA — already resolved, expired, another CLA's transfer — is
    // logged and dropped. The snapshot checks only filter; the
    // status-conditioned swap below is the authoritative arbiter.
    #[cfg_attr(feature = "instrument", instrument(skip_all, fields(bundle.id = %bundle_id)))]
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

        let bundle::BundleStatus::ForwardAckPending { peer } = bundle.status else {
            debug!(
                "Transfer outcome for bundle {bundle_id} that is not awaiting one ({:?}), ignored",
                bundle.status
            );
            return;
        };

        if !cla.owns_peer(peer) {
            // Also fires for a legitimate CLA whose outcome raced the peer's
            // removal, so this is unremarkable rather than a warning
            debug!("Transfer outcome for peer {peer} from a CLA that does not own it, ignored");
            return;
        }

        // Claim the bundle: the snapshot checks above race the peer sweep,
        // the expiry reaper, and duplicate outcomes, and losing the claim
        // means one of them resolved the bundle first.
        match outcome {
            cla::TransferOutcome::Completed => {
                // The terminal claim is a conditional tombstone: a status hop
                // through Dispatching here is recoverable by the dispatch
                // queue's storage poller mid-resolution, driving a duplicate
                // transmission after delivery.
                if !self.store.tombstone_if(&bundle).await {
                    debug!(
                        "Transfer outcome for bundle {bundle_id} lost the resolution race, ignored"
                    );
                    return;
                }

                metrics::counter!("bpa.bundle.forwarded").increment(1);
                self.report_bundle_forwarded(&bundle).await;

                // Don't use drop_bundle() as we do not want to count the Drop as a 'dropped bundle'
                self.report_bundle_deletion(&bundle, ReasonCode::NoAdditionalInformation)
                    .await;
                self.delete_bundle(bundle).await
            }
            cla::TransferOutcome::Failed => {
                if !self
                    .store
                    .swap_status(&mut bundle, &bundle::BundleStatus::Dispatching)
                    .await
                {
                    debug!(
                        "Transfer outcome for bundle {bundle_id} lost the resolution race, ignored"
                    );
                    return;
                }

                metrics::counter!("bpa.bundle.forwarding.failed").increment(1);

                // Bundle-scoped evidence about a single transfer: re-run the
                // routing decision now, rather than parking in Waiting (whose
                // semantic is "nowhere to go") or resetting the whole peer
                // queue (link-scoped evidence). Dispatch parks the bundle in
                // Waiting itself if no route remains, and its expiry
                // checkpoint drops a bundle that expired during the deferred
                // transfer.
                self.dispatch_bundle(bundle).await
            }
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.id())))]
    fn update_extension_blocks(
        &self,
        bundle: &bundle::Bundle,
        source_data: Bytes,
        next_hop: &Eid,
    ) -> Result<(hardy_bpv7::Bundle, Bytes), hardy_bpv7::editor::Error> {
        // We read the cached extension fields (`hop_count` / `age` from
        // `metadata.extensions`) to rebuild the wire blocks, but never write the
        // bumped values back: `forward_bundle` deletes the bundle on a successful
        // send, or returns it to `Waiting` (re-fetched fresh) on failure, so the
        // in-memory cache is never observed again after this rewrite.
        //
        // Editor needs a `&Bundle`, so re-parse structurally.
        // `editor::Error` has several `From` impls so disambiguate explicitly.
        let hardy_bpv7::parse::Parsed {
            data: source_data,
            bundle: raw,
            ..
        } = hardy_bpv7::parse::parse(source_data).map_err(hardy_bpv7::editor::Error::from)?;

        // RFC 9171 §4.2.3-4/-5: report_on_failure MUST NOT be set on any block
        // of an admin-record or anonymous bundle — the receiver has nowhere
        // meaningful to report to, and a conformant parser (ours included)
        // rejects the combination.
        let report_on_failure =
            !bundle.primary().flags.is_admin_record && !bundle.id().source.is_null();

        // Previous Node Block
        let mut editor = hardy_bpv7::editor::Editor::new(&raw, &source_data)
            .insert_block(hardy_bpv7::block::Type::PreviousNode)
            .map_err(|(_, e)| e)?
            .with_flags(hardy_bpv7::block::Flags {
                report_on_failure,
                ..Default::default()
            })
            .with_data(
                hardy_cbor::encode::emit(
                    &self
                        .node_ids
                        .get_admin_endpoint(&bundle.primary().destination),
                )
                .0
                .into(),
            )
            .rebuild();

        // Increment Hop Count
        if let Some(hop_count) = &bundle.metadata.extensions.hop_count {
            editor = editor
                .insert_block(hardy_bpv7::block::Type::HopCount)
                .map_err(|(_, e)| e)?
                .with_flags(hardy_bpv7::block::Flags {
                    report_on_failure,
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
        if bundle.metadata.extensions.age.is_some() || !bundle.id().timestamp.is_clocked() {
            // We have a bundle age block already, or no valid clock at bundle source
            // So we must add an updated bundle age block
            let bundle_age = (time::OffsetDateTime::now_utc() - bundle.creation_time())
                .whole_milliseconds()
                .clamp(0, u64::MAX as i128) as u64;

            editor = editor
                .insert_block(hardy_bpv7::block::Type::BundleAge)
                .map_err(|(_, e)| e)?
                .with_flags(hardy_bpv7::block::Flags {
                    report_on_failure,
                    must_replicate: true,
                    ..Default::default()
                })
                .with_data(hardy_cbor::encode::emit(&bundle_age).0.into())
                .rebuild();
        }

        // Config-driven legacy-EID re-encode: a next hop matching the
        // configured patterns requires 2-element IPN encoding, so Ipn
        // source/destination re-encode as LegacyIpn. Wire adaptation only:
        // the caller installs the rebuilt block map (extents index the
        // re-encoded bytes) but never the rebuilt primary — the record's
        // primary, and with it the bundle id every store operation is keyed
        // on, keeps the canonical encoding.
        if self.ipn_legacy_peers.iter().any(|p| p.matches(next_hop)) {
            if let Eid::Ipn {
                fqnn,
                service_number,
            } = &bundle.id().source
            {
                editor = editor
                    .with_source(Eid::LegacyIpn {
                        fqnn: *fqnn,
                        service_number: *service_number,
                    })
                    .map_err(|(_, e)| e)?;
            }
            if let Eid::Ipn {
                fqnn,
                service_number,
            } = &bundle.primary().destination
            {
                editor = editor
                    .with_destination(Eid::LegacyIpn {
                        fqnn: *fqnn,
                        service_number: *service_number,
                    })
                    .map_err(|(_, e)| e)?;
            }
        }

        // rebuild_bundle() returns a Bundle whose block extents index the
        // rewritten data, keeping the (bundle, data) pair consistent for the
        // Egress filter chain
        let (new_bundle, chunks) = editor.rebuild_bundle()?;

        // Zero-copy in place if `source_data` uniquely owns; otherwise
        // allocates a fresh buffer.
        Ok((
            new_bundle,
            hardy_bpv7::editor::Chunk::flatten_bytes(chunks, source_data),
        ))
    }
}
