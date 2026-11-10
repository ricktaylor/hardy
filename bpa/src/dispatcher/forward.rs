// `EditorError` keeps bpv7's editor error apart from this crate's `Error`,
// in scope through the parent module.
use hardy_bpv7::{bpsec::key::KeySet, editor::Error as EditorError, hop_info::HopInfo};
use hardy_cbor::encode::emit;

use super::{
    output::{LoadedBundle, pull_headers},
    *,
};
use crate::{keys::derive_keys, rebuilt_headers::RebuiltHeaders};

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
        let loaded = pull_headers(&bundle.bpv7, stream, 0).await;

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
            self.offer_to_cla(cla, peer, lane, cla_addr, next_hop, bundle, loaded, seen)
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
        bundle: bundle::Bundle,
        mut loaded: LoadedBundle,
        seen: routing::RibSnapshot,
    ) -> OfferOutcome {
        let chunks = match self.rebuild_headers(&bundle, &loaded.resident, &next_hop) {
            Ok(chunks) => chunks,
            Err(Refusal::ProtectedPrimary) => {
                debug!(
                    "Legacy next hop {next_hop} needs a re-encoded primary, which a BPSec operation covers"
                );
                return OfferOutcome::Dropped(
                    bundle,
                    Some(ReasonCode::UnexpectedSecurityOperation),
                );
            }
            Err(Refusal::UnprotectedPrimary) => {
                debug!(
                    "Refusing to forward bundle: its primary block would leave with no CRC and no BIB"
                );
                return OfferOutcome::Dropped(bundle, Some(ReasonCode::BlockUnintelligible));
            }
        };

        // The rebuilt header region travels as one segment, then the stored
        // payload block as it streams. The record keeps the stored bundle's
        // block index, so every exit below hands back a record that still
        // indexes the stored bytes.
        let head = loaded.resident.clone();
        let mut stream = RebuiltHeaders::new(chunks, &head, &mut loaded);
        let total_len = stream.total_len(bundle.bpv7.encoded_len());
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
                // A cancelled transfer is a failed acceptance, not evidence
                // about the route: re-enter dispatch at once, as the deferred
                // `Failed` outcome does. This handling is deliberately simple
                // and needs to be smarter: every cancellation is retried
                // alike, however often it recurs and whichever side ended the
                // stream, so a CLA that cancels every attempt loops the
                // bundle until it expires.
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

    // The attempt's edits over the stored bundle's resident bytes: the Egress
    // Rewriters, then the per-hop writes, on one editor over the record's
    // block index, rebuilt once into the chunks `RebuiltHeaders` streams. The
    // keys are derived only when a stage reads through them: the Rewriters
    // do, the per-hop writes do not.
    fn rebuild_headers(
        &self,
        bundle: &bundle::Bundle,
        resident: &[u8],
        next_hop: &Eid,
    ) -> Result<Vec<Chunk>, Refusal> {
        let keys = self
            .filters
            .has_egress_rewriters()
            .then(|| derive_keys(&bundle.bpv7, resident, &*self.key_provider));
        let no_keys = KeySet::EMPTY;
        let keys = keys.as_deref().unwrap_or(&no_keys);
        let mut editor = Editor::new(&bundle.bpv7, resident);

        // Nothing at Egress drops a bundle, and the edits are in-memory only,
        // so a re-route runs the Egress chain again with fresh context.
        self.filters
            .rewrite_egress(&mut editor, &bundle.metadata, next_hop, keys);

        // The per-hop writes follow the Rewriters, so they supersede any
        // Rewriter edit to the blocks they write.
        self.update_extension_blocks(bundle, &mut editor, next_hop)?;

        // RFC 9171 §4.3.1, judged on the primary as it would leave: a CRC,
        // or a BIB targeting it. No stage here changes the primary's CRC, so
        // a primary left with neither, its BIB accepted at ingress, is
        // refused rather than given one.
        if self.primary_block_integrity && !editor.primary_has_integrity() {
            return Err(Refusal::UnprotectedPrimary);
        }

        // The bytes were validated at the door and every edit was checked
        // when it was staged, so a failed rebuild is a BPA or bpv7 bug, and
        // fatal.
        Ok(editor
            .rebuild()
            .trace_expect("The egress rewrite failed to rebuild the bundle"))
    }

    // The per-hop writes, on the attempt's editor. The bytes were validated
    // at the door, so any failure but the legacy re-encode's refusal is a
    // BPA bug or storage corruption, and fatal.
    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.id())))]
    fn update_extension_blocks(
        &self,
        bundle: &bundle::Bundle,
        editor: &mut Editor<'_>,
        next_hop: &Eid,
    ) -> Result<(), Refusal> {
        // We read the cached per-hop fields (`hop_count` / `bundle_age` from
        // `metadata.per_hop`) to rebuild the wire blocks, but never write the
        // bumped values back: the rewrite is per-attempt and in-memory only, and
        // the cache mirrors the stored bytes, which stay as received. The cache
        // IS observed again after this rewrite: a park's reaper expiry watch
        // reads `per_hop.bundle_age`, and wants the original, un-bumped value.

        let legacy = self.ipn_legacy_peers.iter().any(|p| p.matches(next_hop));

        // RFC 9171 §4.2.3-4/-5: an admin-record or anonymous bundle's blocks
        // may not request a report on failure, and a conformant parser (ours
        // included) rejects the combination.
        let report_on_failure = !bundle.primary().forbids_report_on_failure();

        // The per-hop blocks below are replaced through `insert_block`. This
        // node is the acceptor of every operation on a Previous Node or
        // Bundle Age block, which it must replace (RFC 9171 §5.4), and the
        // door discharged that duty: ingress accepted each operation or
        // discarded the block (RFC 9172 §5.1.2), and origination refuses a
        // protected one, so a stored bundle's blocks meet no operation here
        // and the editor refuses neither write. A refusal is a BPA bug.

        // Previous Node Block: this node's administrative endpoint in the
        // destination's scheme. A legacy next hop reads ipn EIDs only in the
        // 2-element encoding (RFC 9758 §7.2), so it goes out in that form, as
        // the primary block's EIDs do below.
        let admin_endpoint = self
            .node_ids
            .get_admin_endpoint(&bundle.primary().destination);
        let previous_node = if legacy {
            legacy_ipn(&admin_endpoint).unwrap_or(admin_endpoint)
        } else {
            admin_endpoint
        };
        editor
            .insert_block(block::Type::PreviousNode)
            .trace_expect("The per-hop rewrite's Previous Node write was refused")
            .with_flags(block::Flags {
                report_on_failure,
                ..Default::default()
            })
            .with_data(emit(&previous_node).0.into())
            .rebuild();

        // Increment the Hop Count, where this node is the acceptor of the
        // operations on it: ingress accepted them, and the block meets none
        // here. The increment is a SHOULD (RFC 9171 §4.4.3), so one it is
        // not travels unchanged, with its security operations, rather than
        // holding the bundle back: a block still under a BIB operation,
        // which this node holds no key for or could not accept, is not its
        // to change, one the editor refuses (BCB-covered beside an encrypted
        // BIB that may cover it) cannot be, and one this node could not
        // read at ingress has no cached value. Origination is not a hop: a
        // bundle this node originated leaves with the count it was built
        // with, and the next node makes the first increment.
        let signed = bundle.bpv7.blocks.values().any(|block| {
            block.block_type == block::Type::HopCount && block.bib != block::BibCoverage::None
        });
        let originated = matches!(bundle.metadata.origin(), bundle::Origin::Originated);
        if signed {
            debug!(
                "Hop Count block travels unchanged: under an integrity operation this node did not accept"
            );
        } else if let Some(hop_count) = &bundle.metadata.per_hop.hop_count
            && !originated
        {
            match editor.insert_block(block::Type::HopCount) {
                Ok(block) => block
                    .with_flags(block::Flags {
                        report_on_failure,
                        must_replicate: true,
                        ..Default::default()
                    })
                    .with_data(
                        emit(&HopInfo {
                            limit: hop_count.limit,
                            count: hop_count.count.saturating_add(1),
                        })
                        .0
                        .into(),
                    )
                    .rebuild(),
                Err(e) => debug!("Hop Count block travels unchanged: {e}"),
            }
        }

        // Bundle Age. A bundle that carries one, or has no creation time and
        // so must (RFC 9171 §4.4.2), gets this hop's (§5.4). A bundle marked
        // for one, by ingress for a Bundle Age it discarded or by a
        // Classifier, gets one from the clock too, for a downstream node
        // without one (§5.5).
        let required =
            bundle.metadata.per_hop.bundle_age.is_some() || !bundle.id().timestamp.is_clocked();
        if required || bundle.metadata.add_bundle_age() {
            let bundle_age = (time::OffsetDateTime::now_utc() - bundle.creation_time())
                .whole_milliseconds()
                .clamp(0, u64::MAX as i128) as u64;

            editor
                .insert_block(block::Type::BundleAge)
                .trace_expect("The per-hop rewrite's Bundle Age write was refused")
                .with_flags(block::Flags {
                    report_on_failure,
                    must_replicate: true,
                    ..Default::default()
                })
                .with_data(emit(&bundle_age).0.into())
                .rebuild();
        }

        // Config-driven legacy-EID re-encode: a next hop matching the
        // configured patterns requires 2-element IPN encoding, and a
        // 3-element EID anywhere in the primary makes the whole block
        // undecodable to it (RFC 9758 §7.2), so the source, destination and
        // report-to each re-encode as LegacyIpn where that changes their
        // bytes (see legacy_ipn). A primary with no such EID goes out as it
        // is. Wire adaptation only: the re-encoded bytes travel to the CLA,
        // and the record keeps its stored index and primary — with the
        // latter the bundle id every store operation is keyed on, in the
        // canonical encoding. The editor refuses an edit while a BIB targets
        // the primary or a remaining operation's scope includes it, judged
        // after the per-hop writes above have stripped the blocks they
        // rewrote from their BIBs: an operation the re-encode would break,
        // so the bundle cannot go to this next hop. bpv7 answers for each
        // security context; the BPA reads none.
        if legacy {
            if let Some(source) = legacy_ipn(&bundle.id().source) {
                match editor.set_source(source) {
                    Err(
                        EditorError::PrimaryBlockHasBib | EditorError::PrimaryInSecurityScope(_),
                    ) => return Err(Refusal::ProtectedPrimary),
                    result => result.trace_expect("The legacy re-encode of the source failed"),
                }
            }
            if let Some(destination) = legacy_ipn(&bundle.primary().destination) {
                match editor.set_destination(destination) {
                    Err(
                        EditorError::PrimaryBlockHasBib | EditorError::PrimaryInSecurityScope(_),
                    ) => return Err(Refusal::ProtectedPrimary),
                    result => result.trace_expect("The legacy re-encode of the destination failed"),
                }
            }
            if let Some(report_to) = legacy_ipn(&bundle.primary().report_to) {
                match editor.set_report_to(report_to) {
                    Err(
                        EditorError::PrimaryBlockHasBib | EditorError::PrimaryInSecurityScope(_),
                    ) => return Err(Refusal::ProtectedPrimary),
                    result => result.trace_expect("The legacy re-encode of the report-to failed"),
                }
            }
        }

        Ok(())
    }
}

/// The 2-element `LegacyIpn` form of `eid` for a legacy next hop, where it
/// changes the wire bytes: an `ipn` EID with a non-default allocator. bpv7
/// writes a default-allocator `ipn` EID 2-element already (RFC 9758
/// §6.1.2), and its parser refuses the 3-element form of one as
/// non-canonical, so such an EID needs no edit, and leaves no BPSec
/// operation over the primary block to break.
fn legacy_ipn(eid: &Eid) -> Option<Eid> {
    match eid {
        Eid::Ipn {
            fqnn,
            service_number,
        } if fqnn.allocator_id != 0 => Some(Eid::LegacyIpn {
            fqnn: *fqnn,
            service_number: *service_number,
        }),
        _ => None,
    }
}

/// Why the egress rewrite produced no bundle for this attempt.
enum Refusal {
    /// The next hop needs legacy IPN encoding, and a BPSec operation covers
    /// the primary block the re-encode would change, as its target or
    /// through its scope. Deterministic for this next hop.
    ProtectedPrimary,
    /// The primary block would leave with neither a CRC nor a BIB targeting
    /// it (RFC 9171 §4.3.1). Deterministic for every next hop.
    UnprotectedPrimary,
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroU64;
    #[cfg(feature = "rfc9173")]
    use core::sync::atomic::{AtomicUsize, Ordering};

    #[cfg(feature = "rfc9173")]
    use hardy_bpv7::bpsec::key::KeySource;
    use hardy_bpv7::{
        builder::Builder, bundle::Id, crc::CrcType, creation_timestamp::CreationTimestamp,
        eid::NodeId,
    };

    use super::*;
    #[cfg(feature = "rfc9173")]
    use crate::{
        dispatcher::tests::encrypted_payload_to,
        filter::{RewriteContext, Rewriter, pack::FilterPack},
        keys::KeyProvider,
    };
    use crate::{
        dispatcher::tests::{config, dispatcher, dispatcher_with},
        keys::NullKeyProvider,
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
        let parsed = hardy_bpv7::parse::parse(data).unwrap().bundle;
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

    // Counts the key sources it derives; holds no keys.
    #[cfg(feature = "rfc9173")]
    struct CountingKeys(AtomicUsize);

    #[cfg(feature = "rfc9173")]
    impl KeyProvider for CountingKeys {
        fn key_source(&self, _bundle: &hardy_bpv7::Bundle, _data: &[u8]) -> Box<dyn KeySource> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Box::new(KeySet::EMPTY)
        }
    }

    #[cfg(feature = "rfc9173")]
    struct NoopRewriter;

    #[cfg(feature = "rfc9173")]
    impl Rewriter for NoopRewriter {
        fn rewrite(&self, _ctx: &mut RewriteContext<'_, '_>) {}
    }

    // Two Egress Rewriters that edit nothing.
    #[cfg(feature = "rfc9173")]
    fn noop_egress_pack() -> FilterPack {
        let mut pack = FilterPack::new("test");
        pack.egress_rewriter("noop-a", NoopRewriter);
        pack.egress_rewriter("noop-b", NoopRewriter);
        pack
    }

    // The egress door derives the attempt's key source only when a stage
    // reads through it: for a bundle carrying a BCB, none when no Egress
    // Rewriter is registered, since the per-hop writes read no keys, and one
    // for the whole Egress chain when there are Rewriters.
    #[cfg(feature = "rfc9173")]
    #[tokio::test]
    async fn egress_derives_keys_only_for_a_stage_that_reads_them() {
        for (packs, derived) in [(Vec::new(), 0), (vec![noop_egress_pack()], 1)] {
            let metadata_store = Arc::new(MetadataMemStorage::new(None));
            let data_store = Arc::new(BundleMemStorage::new(None, None));
            let keys = Arc::new(CountingKeys(AtomicUsize::new(0)));
            let dispatcher = dispatcher_with(
                metadata_store.clone(),
                data_store.clone(),
                config(),
                keys.clone(),
                packs,
            )
            .await;

            let (bpv7, data) = encrypted_payload_to("ipn:0.3.2");
            let storage_name = data_store.save(data).await.unwrap();
            let mut metadata = bundle::BundleMetadata::originated();
            metadata.storage_name = Some(storage_name);
            let bundle = bundle::Bundle {
                bpv7,
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
            dispatcher
                .forward_bundle(
                    &RecordingCla { offers_tx },
                    7,
                    None,
                    &cla::ClaAddress::Private("peer".as_bytes().into()),
                    bundle,
                )
                .await;

            assert_eq!(offers_rx.try_recv(), Ok(bundle_id), "the bundle is offered");
            assert_eq!(keys.0.load(Ordering::Relaxed), derived);
        }
    }

    // A bundle whose primary block would leave with neither a CRC nor a BIB,
    // here one stored that way, is refused at the egress door when the node
    // requires primary integrity: dropped, never offered. Without the
    // requirement it travels as stored.
    #[tokio::test]
    async fn egress_refuses_a_primary_with_no_integrity() {
        for (primary_block_integrity, offered) in [(true, false), (false, true)] {
            let metadata_store = Arc::new(MetadataMemStorage::new(None));
            let data_store = Arc::new(BundleMemStorage::new(None, None));
            let dispatcher = dispatcher_with(
                metadata_store.clone(),
                data_store.clone(),
                Config {
                    primary_block_integrity,
                    ..config()
                },
                Arc::new(NullKeyProvider),
                Vec::new(),
            )
            .await;

            let (bpv7, data) =
                Builder::new("ipn:0.2.1".parse().unwrap(), "ipn:0.3.2".parse().unwrap())
                    .with_crc_type(CrcType::None)
                    .with_payload(b"unprotected".to_vec().into())
                    .build(CreationTimestamp::now())
                    .unwrap();
            assert!(!bpv7.primary_has_integrity(), "precondition");
            let storage_name = data_store.save(Bytes::from(data)).await.unwrap();
            let mut metadata = bundle::BundleMetadata::originated();
            metadata.storage_name = Some(storage_name);
            let bundle = bundle::Bundle {
                bpv7,
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
            dispatcher
                .forward_bundle(
                    &RecordingCla { offers_tx },
                    7,
                    None,
                    &cla::ClaAddress::Private("peer".as_bytes().into()),
                    bundle,
                )
                .await;

            assert_eq!(
                offers_rx.try_recv().is_ok(),
                offered,
                "integrity required: {primary_block_integrity}"
            );
            assert!(
                metadata_store.get(&bundle_id).await.unwrap().is_none(),
                "the bundle is resolved, refused or sent"
            );
        }
    }
}
