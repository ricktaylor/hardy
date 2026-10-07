// `EditorError` keeps bpv7's editor error apart from this crate's `Error`,
// in scope through the parent module.
use hardy_bpv7::editor::{Editor, Error as EditorError};

use super::{
    output::{ChunkReceiver, Resident, pull_headers},
    *,
};

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

        // Open the stored bundle, now we know we need it, and hold it until
        // its headers are resident: the payload streams on to the CLA.
        let Some((mut bundle, stream)) = self.load_stream_or_drop(bundle).await else {
            return;
        };
        let resident = pull_headers(&bundle.bpv7, stream).await;

        // Snapshot the routing table before the claim: the parks below
        // re-check it to close the park-vs-poll window (see park_bundle).
        let seen = self.rib.table_snapshot();

        // A transfer commits at the claim below — the reaper defers an
        // in-flight hand-off — so never commence one for a bundle that has
        // already expired: resolve it as the reaper would.
        if bundle.has_expired() {
            return self.drop_bundle(bundle, ReasonCode::LifetimeExpired).await;
        }

        // Claim the bundle out of its peer queue before the in-memory rewrite
        // in the offer and before offering it. The claim must be a conditional
        // swap: the egress channel delivers at-least-once, so a duplicate copy
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

        // Claim-to-resolution is one expression: the offer's outcome is the
        // claim's resolution.
        self.resolve_offer(
            OfferKind::Forward,
            self.offer_to_cla(cla, peer, lane, cla_addr, next_hop, bundle, resident, seen)
                .await,
        )
        .await
    }

    /// Offer a claimed bundle to its CLA. Runs strictly inside the
    /// `ForwardAckPending` claim: every exit is an [`OfferOutcome`] the
    /// caller resolves, so the claim cannot dangle.
    #[allow(clippy::too_many_arguments)]
    async fn offer_to_cla(
        &self,
        cla: &dyn cla::Cla,
        peer: u32,
        lane: Option<u32>,
        cla_addr: &cla::ClaAddress,
        next_hop: Eid,
        mut bundle: bundle::Bundle,
        resident: Resident,
        seen: routing::RibSnapshot,
    ) -> OfferOutcome {
        // One editor serves the attempt, over the record's block index and
        // the resident bytes: the scheduled removals, the Egress Rewriters
        // and the per-hop writes all edit through it, and the bundle is
        // rebuilt once. The keys are derived once, over the stored bundle;
        // the key source is `!Send`, so it stays inside this block, ahead of
        // the CLA's await.
        let chunks = {
            let keys = filter::output_keys(&bundle.bpv7, &resident.bytes, &*self.key_provider);
            let editor = Editor::new(&bundle.bpv7, &resident.bytes);

            // The §E removals the ingress gate deferred apply first, so the
            // Rewriters see the bundle as it will travel, and the strip can never
            // delete a Rewriter's insert into a removed block's number.
            let (mut editor, _) = self.strip_removed_blocks(&bundle, editor, &*keys);

            // Egress chain: the registered Rewriters, on the stripped bundle.
            // Nothing at Egress drops a bundle.
            // - Runs after dequeue from ForwardPending, just before CLA send
            // - Edits are in-memory only (like Deliver), NOT persisted
            // - If send fails or peer goes down, bundle returns to Waiting and may
            //   route to a different peer, so Egress runs again with fresh context
            self.filters
                .run_egress(&mut editor, &bundle.metadata, &next_hop, &*keys);

            // The per-hop writes follow the Rewriters, so they supersede any
            // Rewriter edit to the blocks they write, and the BPSec blocks (BIB/
            // BCB, possibly peer-specific) belong after them.
            let editor = match self.update_extension_blocks(&bundle, editor, &next_hop) {
                Ok(editor) => editor,
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

            // The one rebuild. The edits shift block extents, so the rebuilt block
            // map replaces the old one; its bytes were validated at ingress and
            // every edit was checked when it was staged, so a failure is a BPA or
            // bpv7 bug, and fatal. It is in-memory only: parks persist status
            // alone, and a re-dispatch re-enters from the persisted representation
            // (see park_bundle), so no exit needs to restore the stored map.
            let (rebuilt, chunks) = editor
                .rebuild_bundle()
                .trace_expect("The egress rewrite failed to rebuild the bundle");
            bundle.bpv7.blocks = rebuilt.blocks;
            chunks
        };

        // And pass to CLA: the rebuild's chunks travel as segments, the kept
        // payload streaming on from the store, and the rebuilt block map
        // gives the exact length.
        let total_len = bundle.bpv7.encoded_len();
        let mut stream = ChunkReceiver::new(chunks, resident, total_len);
        match cla
            .forward(lane, cla_addr, bundle.id(), total_len, &mut stream)
            .await
        {
            Ok(cla::ForwardBundleResult::Sent) => OfferOutcome::Completed(bundle),
            Ok(cla::ForwardBundleResult::Accepted) => {
                // The CLA owns the transfer; the bundle stays in
                // ForwardAckPending until the outcome arrives or the peer is
                // removed.
                OfferOutcome::Detached(bundle)
            }
            Ok(cla::ForwardBundleResult::NoNeighbour) => {
                // Link-scoped evidence: the neighbour is gone. Return the
                // bundle to Waiting, and reset the whole peer queue so its
                // bundles await a fresh routing decision alongside it. The
                // sweep touches only ForwardPending, never this claimed
                // bundle, so it can run before the park.
                debug!(
                    "CLA indicates neighbour has gone, clearing queue assignment for peer {peer}"
                );
                self.store.reset_peer_queue(peer).await;
                OfferOutcome::Parked(bundle, bundle::BundleStatus::Waiting, seen)
            }
            Err(cla::Error::StreamCancelled) => {
                // A cancelled transfer is a failed acceptance: the bundle was
                // not forwarded, and is valid to retry, so it re-enters
                // dispatch at once, as a deferred `Failed` outcome does, with
                // no routing or link event; resolve_offer re-claims it and
                // counts the failure. A CLA that cancels every attempt loops
                // until the bundle expires.
                debug!("Transfer to peer {peer} was cancelled, re-dispatching");
                OfferOutcome::Redispatch(bundle)
            }
            Err(e) => {
                metrics::counter!("bpa.bundle.forwarding.failed").increment(1);
                debug!("Failed to forward bundle to peer {peer}: {e}, returning it to Waiting");

                // Bundle-scoped evidence about a single transfer: park only
                // this bundle, leaving the rest of the peer's queue alone —
                // resetting the queue is the response to link-scoped
                // evidence, above. Unlike a cancellation, or the deferred
                // `Failed` outcome, which is paced by a network round trip,
                // any other synchronous failure can be deterministic and
                // instantaneous, so re-running dispatch inline here could
                // spin; the retry waits in Waiting for the next routing or
                // link event — park_bundle re-dispatches at most once, and
                // only if such an event landed while this transfer was in
                // flight.
                OfferOutcome::Parked(bundle, bundle::BundleStatus::Waiting, seen)
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
        let Some(bundle) = self.store.get_metadata(bundle_id).await else {
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
        // the expiry reaper, and duplicate outcomes, and losing the claim —
        // resolve_offer's conditional tombstone for a completion, its
        // Dispatching re-claim for a failure — means one of them resolved
        // the bundle first. A completion must not hop through Dispatching:
        // that status is recoverable by the dispatch queue's storage poller
        // mid-resolution, driving a duplicate transmission after delivery.
        match outcome {
            cla::TransferOutcome::Completed => {
                self.resolve_offer(OfferKind::Forward, OfferOutcome::Completed(bundle))
                    .await
            }
            cla::TransferOutcome::Failed => {
                // Bundle-scoped evidence about a single transfer: re-run the
                // routing decision now, rather than parking in Waiting (whose
                // semantic is "nowhere to go") or resetting the whole peer
                // queue (link-scoped evidence). Dispatch parks the bundle in
                // Waiting itself if no route remains, and its expiry
                // checkpoint drops a bundle that expired during the deferred
                // transfer.
                self.resolve_offer(OfferKind::Forward, OfferOutcome::Redispatch(bundle))
                    .await
            }
        }
    }

    // The per-hop writes, on the attempt's editor after the scheduled removals
    // and the Egress Rewriters. The editor works over the record's block
    // index, whose coverage stamps are the ones ingress derived: keyed for a
    // key holder, so this node edits the blocks it has proven no encrypted
    // BIB covers. The bytes were validated at ingress, so any failure other
    // than a PerHopRefusal is a BPA bug or storage corruption, and fatal.
    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.id())))]
    fn update_extension_blocks<'a>(
        &self,
        bundle: &bundle::Bundle,
        editor: Editor<'a>,
        next_hop: &Eid,
    ) -> Result<Editor<'a>, PerHopRefusal> {
        // We read the cached extension fields (`hop_count` / `age` from
        // `metadata.extensions`) to rebuild the wire blocks, but never write the
        // bumped values back: the rewrite is per-attempt and in-memory only, and
        // the cache mirrors the stored bytes, which stay as received. The cache
        // IS observed again after this rewrite: a park's reaper expiry watch
        // reads `extensions.age`, and wants the original, un-bumped value.
        let legacy = self.ipn_legacy_peers.iter().any(|p| p.matches(next_hop));

        // RFC 9171 §4.2.3-4/-5: an admin-record or anonymous bundle's blocks
        // may not request a report on failure, and a conformant parser (ours
        // included) rejects the combination.
        let report_on_failure = !bundle.primary().forbids_report_on_failure();

        // The per-hop blocks below are replaced through `insert_block`, which
        // strips a replaced block from any plaintext BIB that covers it. That
        // is Hardy's relay policy, within the waypoint latitude of RFC 9172
        // §3.1: this node must rewrite these blocks (RFC 9171 §5.4; the hop
        // count's increment is a §4.4.3 SHOULD), and no operation over the
        // old body survives the rewrite. Stripping keeps the sender's other
        // results verifiable, where a stale result would fail verification
        // downstream and cost the receiver the block or the whole bundle
        // under its failure policy. Where this node is the operation's
        // security acceptor and verified it at ingress, as for a hop-by-hop
        // PreviousNode signature, the strip also discharges the RFC 9172
        // §5.1.2 acceptor duty; elsewhere the node in effect acts as the
        // acceptor of an operation it may not have verified. A BCB-covered
        // block is stripped from its BCB and written in plaintext: this node,
        // required to update the block, is the acceptor of its confidentiality
        // operation too (forwarder policy). The editor refuses a block whose
        // coverage reads `BibCoverage::Maybe` — BCB-covered beside an
        // encrypted BIB this node could not decrypt at ingress, so the BIB may
        // cover it — or one an encrypted BIB is known to cover, whose
        // operation cannot be stripped from the ciphertext. A refused
        // Previous Node or Bundle Age write parks the bundle: both are MUSTs
        // (RFC 9171 §5.4).
        let covered = |(_, e)| PerHopRefusal::Covered(e);

        // Previous Node Block
        let mut editor = editor
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

        // Increment Hop Count, where the block can be updated. The increment
        // is a SHOULD (RFC 9171 §4.4.3): a Hop Count block the editor refuses
        // (one an encrypted BIB may or does cover) travels
        // unchanged, with its security operations, rather than holding the
        // bundle back. One this node could not read at ingress has no cached
        // value, and travels unchanged too. Origination is not a hop: a bundle
        // this node originated leaves with the count it was built with, and
        // the next node makes the first increment.
        let originated = matches!(bundle.metadata.origin(), bundle::Origin::Originated);
        if let Some(hop_count) = &bundle.metadata.extensions.hop_count
            && !originated
        {
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

        // Update Bundle Age, if required
        if bundle.metadata.extensions.age.is_some() || !bundle.id().timestamp.is_clocked() {
            // We have a bundle age block already, or no valid clock at bundle source
            // So we must add an updated bundle age block
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

        // Config-driven legacy-EID re-encode: a next hop matching the
        // configured patterns requires 2-element IPN encoding, so Ipn
        // source/destination re-encode as LegacyIpn. Wire adaptation only:
        // the caller installs the rebuilt block map (extents index the
        // re-encoded bytes) but never the rebuilt primary — the record's
        // primary, and with it the bundle id every store operation is keyed
        // on, keeps the canonical encoding. The editor refuses the edit
        // while a BIB targets the primary or a remaining operation's scope
        // includes it, judged after the per-hop writes above have stripped
        // the blocks they rewrote from their BIBs: an operation the
        // re-encode would break, so the bundle cannot go to this next hop.
        // bpv7 answers for each security context; the BPA reads none.
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

        Ok(editor)
    }
}

/// Why the per-hop rewrite produced no bundle for this attempt.
enum PerHopRefusal {
    /// The next hop needs legacy IPN encoding, and a BPSec operation covers
    /// the primary block the re-encode would change, as its target or
    /// through its scope: deterministic for this next hop.
    ProtectedPrimary,
    /// A per-hop block is under BPSec coverage the keyless rewrite cannot
    /// safely update.
    Covered(EditorError),
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroU64;

    use hardy_bpv7::{bundle::Id, eid::NodeId};

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
        async fn on_register(
            &self,
            _sink: Box<dyn cla::Sink>,
            _node_ids: &[NodeId],
            _max_bundle_size: Option<NonZeroU64>,
        ) {
        }

        async fn on_unregister(&self) {}

        async fn forward(
            &self,
            _lane: Option<u32>,
            _cla_addr: &cla::ClaAddress,
            bundle_id: &Id,
            _total_len: u64,
            _stream: &mut dyn crate::stream::Receiver<cla::Segment>,
        ) -> cla::Result<cla::ForwardBundleResult> {
            let _ = self.offers_tx.send(bundle_id.clone());
            Ok(cla::ForwardBundleResult::Sent)
        }
    }

    /// `forward_bundle` never commences a transfer for an expired bundle:
    /// the pre-claim expiry checkpoint resolves it as `LifetimeExpired`
    /// before the CLA sees an offer. Driven as a direct call because this
    /// is exactly the egress fast path's shape — a bundle that expired
    /// while queued arrives here from the in-memory buffer, bypassing the
    /// storage poller's expiry filter.
    #[tokio::test]
    async fn expired_queued_bundle_is_never_offered() {
        let metadata_store = Arc::new(MetadataMemStorage::new(None));
        let data_store = Arc::new(BundleMemStorage::new(None, None));
        let dispatcher = dispatcher(metadata_store.clone(), data_store.clone()).await;

        // Seed the record exactly as the egress queue holds it: data
        // stored, metadata parked in ForwardPending, expired at build.
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
                &cla::ClaAddress::Private("peer".as_bytes().into()),
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
}
