// `EditorError` keeps bpv7's editor error apart from this crate's `Error`,
// in scope through the parent module.
use hardy_bpv7::editor::Error as EditorError;

use super::*;

impl Dispatcher {
    #[cfg_attr(feature = "instrument", instrument(skip(self,cla,bundle),fields(bundle.id = %bundle.id())))]
    pub async fn forward_bundle(
        &self,
        cla: &dyn cla::Cla,
        peer: u32,
        lane: Option<u32>,
        cla_addr: &cla::Address,
        bundle: bundle::Bundle,
    ) {
        // The queue assignment carries the resolved adjacency, and the
        // claim below overwrites the status, so take it first. The egress
        // channel only delivers this queue's assignments, so any other
        // status here is a stale copy whose owner resolves it elsewhere.
        let bundle::BundleStatus::ForwardPending { next_hop, .. } = &bundle.status else {
            debug!("Bundle reached forwarding without a queue assignment, dropping copy");
            return;
        };
        let next_hop = next_hop.clone();

        // The data is needed only now the bundle is going out.
        let Some((mut bundle, data)) = self.load_data_or_drop(bundle).await else {
            return;
        };

        // Snapshot the routing table before the claim, since the parks
        // below re-check it (see `park_bundle`).
        let seen = self.rib.table_snapshot();

        // The reaper defers an in-flight hand-off, so an already expired
        // bundle is resolved here as the reaper would, not offered.
        if bundle.has_expired() {
            return self.drop_bundle(bundle, ReasonCode::LifetimeExpired).await;
        }

        // Claim the bundle out of its peer queue before the offer rewrites
        // it in memory. The claim is a conditional swap, because the egress
        // channel delivers at least once and a duplicate recovered by the
        // storage poller must lose here. It precedes the offer because a
        // deferred outcome can land the instant the CLA accepts, and
        // `transfer_outcome` honours only a bundle already in
        // `ForwardAckPending`, and because the persist needs metadata that
        // still indexes the stored data. The status also tells an in-flight
        // transfer from a queued one, so `reset_peer_queue` cannot race the
        // offer.
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

        // The offer's outcome resolves the claim.
        self.resolve_offer(
            OfferKind::Forward,
            self.offer_to_cla(cla, peer, lane, cla_addr, next_hop, bundle, data, seen)
                .await,
        )
        .await
    }

    /// Offers a claimed bundle to its CLA.
    ///
    /// This runs inside the `ForwardAckPending` claim, and every exit is an
    /// [`OfferOutcome`] the caller resolves, so the claim cannot dangle.
    #[allow(clippy::too_many_arguments)]
    async fn offer_to_cla(
        &self,
        cla: &dyn cla::Cla,
        peer: u32,
        lane: Option<u32>,
        cla_addr: &cla::Address,
        next_hop: Eid,
        mut bundle: bundle::Bundle,
        data: Bytes,
        seen: routing::RibSnapshot,
    ) -> OfferOutcome {
        // The block removals ingress deferred apply first, so the
        // rewriters see the bundle as it travels and the strip cannot
        // delete a rewriter's insert into a removed block's number.
        let data = self.strip_removed_blocks(&mut bundle, data);

        // The registered egress rewriters run on the stripped wire form.
        // Nothing at egress drops a bundle, so the chain hands the pair
        // back. Their edits are in memory only, as at delivery, so a bundle
        // whose send fails returns to `Waiting` and runs the chain again
        // against whichever peer it routes to next.
        let (mut bundle, data) =
            self.filters
                .run_egress(bundle, data, &next_hop, &*self.key_provider);

        // The per-hop writes follow the rewriters, so they supersede any
        // rewriter edit to the blocks they write, and the BPSec blocks,
        // which may be peer-specific, come after them. The rewrite shifts
        // block extents, so the rebuilt block map replaces the old one. It
        // too is in memory only: a park persists the status alone and a
        // re-dispatch re-enters from the stored bundle, so no exit has to
        // restore the stored map.
        let mut data = match self.update_extension_blocks(&bundle, data, &next_hop) {
            Ok((new_bundle, data)) => {
                bundle.bpv7.blocks = new_bundle.blocks;
                data
            }
            Err(PerHopRefusal::ProtectedPrimary) => {
                debug!(
                    "Legacy next hop {next_hop} needs a re-encoded primary, which a BPSec operation covers"
                );
                return OfferOutcome::Dropped(
                    bundle,
                    Some(ReasonCode::UnexpectedSecurityOperation),
                );
            }
            Err(PerHopRefusal::Covered(e)) => {
                warn!("Failed to update extension blocks: {e}");
                return OfferOutcome::Parked(bundle, bundle::BundleStatus::Waiting, seen);
            }
        };

        // The whole bundle is in hand, so it travels as a single final
        // segment.
        let total_len = data.len() as u64;
        match cla
            .forward(lane, cla_addr, bundle.id(), total_len, &mut data)
            .await
        {
            Ok(cla::ForwardOutcome::Sent) => OfferOutcome::Completed(bundle),
            Ok(cla::ForwardOutcome::Accepted) => {
                // The CLA owns the transfer, and the bundle stays in
                // `ForwardAckPending` until the outcome arrives or the peer
                // is removed.
                OfferOutcome::Detached(bundle)
            }
            Ok(cla::ForwardOutcome::NoNeighbour) => {
                // The neighbour is gone, which is evidence about the link
                // rather than this bundle. Return the bundle to `Waiting`
                // and reset the whole peer queue, so its bundles await a
                // fresh routing decision alongside it. The sweep touches
                // only `ForwardPending`, never this claimed bundle, so it
                // can run before the park.
                debug!(
                    "CLA indicates neighbour has gone, clearing queue assignment for peer {peer}"
                );
                self.store.reset_peer_queue(peer).await;
                OfferOutcome::Parked(bundle, bundle::BundleStatus::Waiting, seen)
            }
            Err(e) => {
                metrics::counter!("bpa.bundle.forwarding.failed").increment(1);
                debug!("Failed to forward bundle to peer {peer}: {e}, returning it to Waiting");

                // The failure is evidence about this transfer alone, so
                // park this bundle and leave the rest of the peer's queue
                // alone; resetting the queue answers evidence about the
                // link, above. Unlike a deferred `Failed` outcome, which a
                // network round trip paces, a synchronous failure can be
                // deterministic and immediate, so re-running dispatch
                // inline could spin. The retry waits in `Waiting` for the
                // next routing or link event, and `park_bundle`
                // re-dispatches at most once, and only if such an event
                // landed while this transfer was in flight.
                OfferOutcome::Parked(bundle, bundle::BundleStatus::Waiting, seen)
            }
        }
    }

    // Resolves a deferred transfer outcome `cla` reports for a bundle it
    // answered `Accepted`. A bundle not currently in `ForwardAckPending`
    // through a peer of the reporting CLA is a stale outcome, logged and
    // dropped. The checks against the snapshot only filter; the
    // status-conditioned swap below arbitrates.
    #[cfg_attr(feature = "instrument", instrument(skip_all, fields(bundle.id = %bundle_id)))]
    pub async fn transfer_outcome(
        &self,
        cla: &cla::table::Record,
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
            // A legitimate CLA whose outcome raced the peer's removal
            // arrives here too, so this is unremarkable rather than a
            // warning.
            debug!("Transfer outcome for peer {peer} from a CLA that does not own it, ignored");
            return;
        }

        // Claim the bundle. The checks above race the peer sweep, the
        // expiry reaper and duplicate outcomes, so losing the claim means
        // one of them resolved the bundle first. A completion claims
        // through `resolve_offer`'s conditional tombstone, a failure
        // through the `Dispatching` swap below; a completion must not pass
        // through `Dispatching`, which the dispatch queue's storage poller
        // recovers mid-resolution and transmits again after delivery.
        match outcome {
            cla::TransferOutcome::Completed => {
                self.resolve_offer(OfferKind::Forward, OfferOutcome::Completed(bundle))
                    .await
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

                // The failure is evidence about this transfer alone, so
                // re-run the routing decision rather than park in
                // `Waiting`, which means nowhere to go, or reset the peer
                // queue, which answers evidence about the link. Dispatch
                // parks the bundle itself where no route remains, and its
                // expiry checkpoint drops one that expired during the
                // deferred transfer.
                self.resolve_offer(OfferKind::Forward, OfferOutcome::Redispatch(bundle))
                    .await
            }
        }
    }

    // The per-hop rewrite, over the egress rewriters' output. Its bytes
    // were validated at ingress, so any failure other than a
    // `PerHopRefusal` is a BPA bug or storage corruption, and fatal.
    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.id())))]
    fn update_extension_blocks(
        &self,
        bundle: &bundle::Bundle,
        source_data: Bytes,
        next_hop: &Eid,
    ) -> Result<(hardy_bpv7::Bundle, Bytes), PerHopRefusal> {
        // The cached `hop_count` and `age` in `metadata.extensions`
        // rebuild the wire blocks, and the new values are never written
        // back: the rewrite serves one attempt and lives in memory, while
        // the cache mirrors the stored bytes, which stay as received. A
        // park's reaper expiry watch reads `extensions.age` after this
        // rewrite, and needs the original value.
        //
        // `Editor` takes a `&Bundle`, so the bytes are parsed again.
        let hardy_bpv7::parse::Parsed {
            data: source_data,
            bundle: raw,
            ..
        } = hardy_bpv7::parse::parse(source_data)
            .trace_expect("The per-hop rewrite's bytes do not decode");

        let legacy = self.ipn_legacy_peers.iter().any(|p| p.matches(next_hop));

        // RFC 9171 Section 4.2.3: the blocks of an administrative record
        // or an anonymous bundle may not request a report on failure, and a
        // conformant parser, this one included, rejects the combination.
        let report_on_failure = !bundle.primary().forbids_report_on_failure();

        // The per-hop blocks below are replaced through `insert_block`, which
        // strips a replaced block from any plaintext BIB that covers it. That
        // is Hardy's relay policy, within the waypoint latitude of RFC 9172
        // Section 3.1: this node must rewrite these blocks (RFC 9171
        // Section 5.4, the hop count's increment being a SHOULD of Section
        // 4.4.3), and no operation over the old body survives the rewrite. Stripping keeps the sender's other
        // results verifiable, where a stale result would fail verification
        // downstream and cost the receiver the block or the whole bundle
        // under its failure policy. Where this node is the operation's
        // security acceptor and verified it at ingress, as for a hop-by-hop
        // PreviousNode signature, the strip also discharges the acceptor
        // duty of RFC 9172 Section 5.1.2; elsewhere the node in effect acts
        // as the acceptor of an operation it may not have verified. A
        // Previous Node or Bundle Age block the editor cannot safely update,
        // being BCB-covered and so possibly under an encrypted BIB, refuses,
        // and the caller parks the bundle, both writes being MUSTs (RFC 9171
        // Section 5.4).
        let covered = |(_, e)| PerHopRefusal::Covered(e);

        let mut editor = hardy_bpv7::editor::Editor::new(&raw, &source_data)
            .insert_block(hardy_bpv7::block::Type::PreviousNode)
            .map_err(covered)?
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

        // Increment Hop Count, where the block can be updated. The
        // increment is a SHOULD (RFC 9171 Section 4.4.3), so a Hop Count
        // block the editor refuses, being BCB-covered beside an encrypted
        // BIB that may cover it, travels unchanged, with its security
        // operations, rather than holding the bundle back. One this node could not read at ingress has no cached
        // value, and travels unchanged too.
        if let Some(hop_count) = &bundle.metadata.extensions.hop_count {
            editor = match editor.insert_block(hardy_bpv7::block::Type::HopCount) {
                Ok(block) => block
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
                    .rebuild(),
                Err((editor, e)) => {
                    debug!("Hop Count block travels unchanged: {e}");
                    editor
                }
            };
        }

        if bundle.metadata.extensions.age.is_some() || !bundle.id().timestamp.is_clocked() {
            // The bundle already carries an age block, or its source had
            // no clock, so the age block is rewritten.
            let bundle_age = (time::OffsetDateTime::now_utc() - bundle.creation_time())
                .whole_milliseconds()
                .clamp(0, u64::MAX as i128) as u64;

            editor = editor
                .insert_block(hardy_bpv7::block::Type::BundleAge)
                .map_err(covered)?
                .with_flags(hardy_bpv7::block::Flags {
                    report_on_failure,
                    must_replicate: true,
                    ..Default::default()
                })
                .with_data(hardy_cbor::encode::emit(&bundle_age).0.into())
                .rebuild();
        }

        // A next hop matching the configured patterns needs 2-element IPN
        // encoding, so an `Ipn` source and destination re-encode as
        // `LegacyIpn`. The adaptation is to the wire form only: the caller
        // installs the rebuilt block map, whose extents index the
        // re-encoded bytes, but never the rebuilt primary, so the record's
        // primary, and the bundle id every store operation keys on, keep
        // the canonical encoding. The editor refuses the edit while a BIB
        // targets the primary or a remaining operation's scope covers it,
        // judged after the per-hop writes have stripped the blocks they
        // rewrote from their BIBs. Such an operation would not survive the
        // re-encode, so the bundle cannot go to this next hop.
        if legacy {
            if let Eid::Ipn {
                fqnn,
                service_number,
            } = &bundle.id().source
            {
                editor = match editor.with_source(Eid::LegacyIpn {
                    fqnn: *fqnn,
                    service_number: *service_number,
                }) {
                    Err((
                        _,
                        EditorError::PrimaryBlockHasBib | EditorError::PrimaryInSecurityScope(_),
                    )) => return Err(PerHopRefusal::ProtectedPrimary),
                    result => result
                        .map_err(|(_, e)| e)
                        .trace_expect("The legacy re-encode of the source failed"),
                };
            }
            if let Eid::Ipn {
                fqnn,
                service_number,
            } = &bundle.primary().destination
            {
                editor = match editor.with_destination(Eid::LegacyIpn {
                    fqnn: *fqnn,
                    service_number: *service_number,
                }) {
                    Err((
                        _,
                        EditorError::PrimaryBlockHasBib | EditorError::PrimaryInSecurityScope(_),
                    )) => return Err(PerHopRefusal::ProtectedPrimary),
                    result => result
                        .map_err(|(_, e)| e)
                        .trace_expect("The legacy re-encode of the destination failed"),
                };
            }
        }

        // `rebuild_bundle` returns a bundle whose block extents index the
        // rewritten data, so the pair stays consistent for the hand-off to
        // the CLA.
        let (new_bundle, chunks) = editor
            .rebuild_bundle()
            .trace_expect("The per-hop rewrite failed to rebuild the bundle");

        // This writes in place where `source_data` is the unique owner,
        // and allocates otherwise.
        Ok((
            new_bundle,
            hardy_bpv7::editor::Chunk::flatten_bytes(chunks, source_data),
        ))
    }
}

/// The reason the per-hop rewrite produced no bundle for this attempt.
enum PerHopRefusal {
    /// The next hop needs legacy IPN encoding, and a BPSec operation
    /// covers the primary block the re-encode would change, as its target
    /// or through its scope. The refusal is the same at every attempt for
    /// this next hop.
    ProtectedPrimary,
    /// A per-hop block is under BPSec coverage the keyless rewrite cannot
    /// safely update.
    Covered(EditorError),
}

#[cfg(test)]
mod tests {

    use hardy_bpv7::bundle::Id;

    use super::*;
    use crate::{
        dispatcher::tests::dispatcher,
        storage::{BundleMemStorage, BundleStorage, MetadataMemStorage, MetadataStorage},
    };

    struct RecordingCla {
        offers_tx: flume::Sender<Id>,
    }

    #[async_trait]
    impl cla::Cla for RecordingCla {
        async fn forward(
            &self,
            _lane: Option<u32>,
            _cla_addr: &cla::Address,
            bundle_id: &Id,
            _total_len: u64,
            _stream: &mut dyn crate::stream::Receiver<cla::Segment>,
        ) -> cla::Result<cla::ForwardOutcome> {
            let _ = self.offers_tx.send(bundle_id.clone());
            Ok(cla::ForwardOutcome::Sent)
        }
    }

    // `forward_bundle` commences no transfer for an expired bundle: the
    // expiry checkpoint before the claim resolves it as `LifetimeExpired`
    // first. Calling it directly is the shape of the egress fast path,
    // where a bundle that expired while queued arrives from the in-memory
    // buffer and bypasses the storage poller's expiry filter.
    #[tokio::test]
    async fn expired_queued_bundle_is_never_offered() {
        let metadata_store = Arc::new(MetadataMemStorage::new(None));
        let data_store = Arc::new(BundleMemStorage::new(None, None));
        let dispatcher = dispatcher(metadata_store.clone(), data_store.clone()).await;

        // Seed the record as the egress queue holds it: the data stored,
        // the metadata parked in `ForwardPending`, and the bundle already
        // expired.
        let past = time::OffsetDateTime::now_utc() - time::Duration::hours(1);
        let timestamp = hardy_bpv7::creation_timestamp::CreationTimestamp::from_parts(
            Some(hardy_bpv7::dtn_time::DtnTime::saturating_from(past)),
            1,
        );
        let (_, data) = hardy_bpv7::builder::Builder::new(
            "ipn:0.2.1".parse().unwrap(),
            "ipn:0.3.2".parse().unwrap(),
        )
        .with_lifetime(core::time::Duration::from_secs(60))
        .with_payload(b"expired in queue".to_vec().into())
        .build(timestamp)
        .unwrap();
        let data = Bytes::from(data);
        let storage_name = data_store.save(data.clone()).await.unwrap();
        let parsed =
            crate::bundle::parse::parse_validate_with_provider(data, hardy_bpv7::bpsec::no_keys)
                .unwrap();
        let mut metadata = bundle::BundleMetadata::originated();
        metadata.storage_name = Some(storage_name);
        let bundle = bundle::Bundle {
            bpv7: parsed,
            metadata,
            status: bundle::BundleStatus::ForwardPending {
                peer: 7,
                queue: 0,
                next_hop: "ipn:0.3.0".parse().unwrap(),
            },
        };
        let bundle_id = bundle.id().clone();
        assert!(metadata_store.insert(&bundle).await.unwrap());

        let (offers_tx, offers_rx) = flume::bounded(16);
        let cla = RecordingCla { offers_tx };
        dispatcher
            .forward_bundle(
                &cla,
                7,
                None,
                &cla::Address::Private("peer".as_bytes().into()),
                bundle,
            )
            .await;

        assert!(
            offers_rx.try_recv().is_err(),
            "an expired bundle must never be offered to the CLA"
        );
        assert!(
            metadata_store.get(&bundle_id).await.unwrap().is_none(),
            "the expired bundle is resolved terminally as LifetimeExpired"
        );
    }

    // The per-hop rewrite's bytes were validated at ingress and come back
    // from storage, so failing to decode them is a BPA bug or storage
    // corruption, and fatal rather than a park. The call is direct because
    // a panic inside a running BPA aborts the test process.
    #[tokio::test]
    #[should_panic(expected = "The per-hop rewrite's bytes do not decode")]
    async fn undecodable_per_hop_bytes_are_fatal() {
        let dispatcher = dispatcher(
            Arc::new(MetadataMemStorage::new(None)),
            Arc::new(BundleMemStorage::new(None, None)),
        )
        .await;
        let (_, data) = hardy_bpv7::builder::Builder::new(
            "ipn:0.2.1".parse().unwrap(),
            "ipn:0.3.2".parse().unwrap(),
        )
        .with_payload(b"truncated in storage".to_vec().into())
        .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
        .unwrap();
        let data = Bytes::from(data);
        let bundle = bundle::Bundle {
            bpv7: crate::bundle::parse::parse_validate_with_provider(
                data.clone(),
                hardy_bpv7::bpsec::no_keys,
            )
            .unwrap(),
            metadata: bundle::BundleMetadata::originated(),
            status: bundle::BundleStatus::ForwardAckPending { peer: 7 },
        };

        let _ = dispatcher.update_extension_blocks(
            &bundle,
            data.slice(..data.len() - 1),
            &"ipn:0.3.0".parse().unwrap(),
        );
    }
}
