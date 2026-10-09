// `Bpv7Error` disambiguates the bpv7 wire-format error from this crate's
// `Error` in scope via the parent module.
use hardy_bpv7::{
    Error as Bpv7Error,
    block::Payload,
    bpsec,
    editor::{Chunk, Editor},
    parse::{self, Parsed},
    status_report::ReasonCode,
};

use super::*;
use crate::services::registry::{Service, ServiceImpl};

impl Dispatcher {
    #[cfg_attr(feature = "instrument", instrument(skip(self, bundle),fields(bundle.id = %bundle.id())))]
    pub(super) async fn deliver_bundle(&self, service: Arc<Service>, bundle: bundle::Bundle) {
        let Some((mut bundle, data)) = self.load_data_or_drop(bundle).await else {
            return;
        };

        // The claim key and every park in the offer use the canonical
        // registration EID stored at construction — the exact key
        // `poll_service_waiting` matches on re-registration. The bundle's
        // own destination can be a different Eid variant for the same
        // endpoint (e.g. LegacyIpn vs Ipn) and would never match.
        let service_eid = service.eid().clone();

        // Snapshot the routing table before the claim: the parks in the
        // offer re-check it to close the park-vs-poll window (see
        // park_bundle).
        let seen = self.rib.table_snapshot();

        // Delivery commits at the claim below — the reaper defers an
        // in-flight delivery — so never commence one for a bundle that has
        // already expired: resolve it as the reaper would.
        if bundle.has_expired() {
            return self.drop_bundle(bundle, ReasonCode::LifetimeExpired).await;
        }

        // Claim the bundle out of its delivery queue before offering it.
        // The claim must be a conditional swap: the delivery channel is
        // at-least-once, so a duplicate copy recovered by the storage
        // poller must lose here rather than produce a second delivery. The
        // new status also marks the point past which the delivery cannot be
        // recalled: the reaper defers it, and the unregister sweep only
        // touches the queued status.
        if !self
            .store
            .swap_status(
                &mut bundle,
                &bundle::BundleStatus::DeliveryAckPending {
                    service: service_eid.clone(),
                },
            )
            .await
        {
            debug!("Bundle already claimed for delivery or swept, skipping offer");
            return;
        }

        // Claim-to-resolution is one expression: the offer's outcome is the
        // claim's resolution.
        self.resolve_offer(
            OfferKind::Delivery,
            self.offer_to_service(service, service_eid, bundle, data, seen)
                .await,
        )
        .await
    }

    /// Offer a claimed bundle to its service. Runs strictly inside the
    /// `DeliveryAckPending` claim: every exit is an [`OfferOutcome`] the
    /// caller resolves, so the claim cannot dangle.
    async fn offer_to_service(
        &self,
        service: Arc<Service>,
        service_eid: Eid,
        mut bundle: bundle::Bundle,
        data: Bytes,
        seen: routing::RibSnapshot,
    ) -> OfferOutcome {
        // One editor serves the scheduled removals and the Deliver Rewriters,
        // over the record's block index and the loaded bytes, and is rebuilt
        // once, only if either edited; with nothing scheduled and no
        // Rewriters, none is built. The keys are derived once, over the stored
        // bundle; the key source is `!Send`, so it stays inside this block,
        // ahead of the service's await.
        let mut data = {
            let keys = filter::output_keys(&bundle.bpv7, &data, &*self.key_provider);
            let data =
                if bundle.metadata.to_remove.is_empty() && !self.filters.has_deliver_rewriters() {
                    data
                } else {
                    let editor = Editor::new(&bundle.bpv7, &data);

                    // The §E removals the ingress gate deferred apply first, so the
                    // Deliver chain sees the bundle as it will be delivered, and the
                    // strip can never delete a Rewriter's insert into a removed
                    // block's number.
                    let (mut editor, stripped) = self.strip_removed_blocks(&bundle, editor, &*keys);

                    // Deliver chain, first stage: the Rewriters (transport-block
                    // strip).
                    let rewritten = self
                        .filters
                        .run_deliver(&mut editor, &bundle.metadata, &*keys);

                    // The bytes were validated at ingress and every edit was checked
                    // when it was staged, so a failed rebuild is a BPA or bpv7 bug.
                    let rebuilt = (stripped || rewritten).then(|| {
                        editor
                            .rebuild_bundle()
                            .trace_expect("The delivery rewrite failed to rebuild the bundle")
                    });
                    match rebuilt {
                        Some((rebuilt, chunks)) => {
                            bundle.bpv7.blocks = rebuilt.blocks;
                            Chunk::flatten_bytes(chunks, data)
                        }
                        None => data,
                    }
                };

            // Deliver chain, second stage: the Verifiers, over the bundle as it
            // will be delivered.
            if let Some(reason) = self.filters.verify_deliver(&bundle, &data, &*keys) {
                return OfferOutcome::Dropped(bundle, reason);
            }
            data
        };

        let delivery_result = match &service.service {
            ServiceImpl::LowLevel(svc) => {
                // Pass raw bundle bytes to low-level services: the whole
                // bundle is in hand, so it travels as a single Final segment.
                let total_len = data.len() as u64;
                svc.on_deliver(bundle.id(), bundle.expiry(), total_len, &mut data)
                    .await
            }
            ServiceImpl::Application(app) => {
                // Extract and decrypt payload for Application.
                // KeyProvider needs a &Bundle; scope the parse
                // as a match expression so the parse OperationSets
                // (which contain `Rc<…>` and are therefore `!Send`) are
                // dropped at the arm boundary, before any `.await` in
                // this async fn. Consume `data` into the parse and work
                // from the authoritative buffer it returns (the streaming
                // path concatenates pushes), converting the payload to an
                // owned `Bytes` (zero-copy for the unencrypted case via
                // `slice_ref`) before the arm ends.
                let payload_result = match parse::parse(data) {
                    Ok(Parsed {
                        data: buf,
                        bundle: raw,
                        bcbs: bcb_ops,
                        ..
                    }) => {
                        let key_source = self.key_source(&raw, &buf);
                        match bpsec::DecryptingReader::new(
                            &raw.blocks,
                            &buf,
                            &bcb_ops,
                            &*key_source,
                        )
                        .into_block_data(1)
                        .and_then(|p| p.ok_or(Bpv7Error::Altered))
                        {
                            Ok(Payload::Borrowed(s)) => Ok(buf.slice_ref(s)),
                            Ok(Payload::Decrypted(d)) => Ok(Bytes::from_owner(d)),
                            Err(e) => Err(e),
                        }
                    }
                    Err(e) => Err(e),
                };

                let mut payload = match payload_result {
                    Err(Bpv7Error::InvalidBPSec(bpsec::Error::NoKey)) => {
                        // TODO: We are unable to decrypt the payload, what do we do?
                        // For now, park for the next registration (which may
                        // bring usable keys).
                        debug!("Failed to decrypt payload: No valid keys");
                        return OfferOutcome::Parked(
                            bundle,
                            bundle::BundleStatus::WaitingForService {
                                service: service_eid,
                            },
                            seen,
                        );
                    }
                    Err(e) => {
                        // Other decryption error - skip delivery
                        debug!("Received an invalid payload: {e}");

                        // TODO: This is where we can wrap the damaged bundle in a "Junk Bundle Payload" and forward it to a 'lost+found' endpoint.  For now we just drop it.

                        return OfferOutcome::Dropped(
                            bundle,
                            Some(ReasonCode::BlockUnintelligible),
                        );
                    }
                    Ok(payload) => payload,
                };

                // As for low-level services, the whole payload is in hand,
                // so it travels as a single Final segment.
                let total_len = payload.len() as u64;
                app.on_deliver(
                    bundle.id(),
                    bundle.expiry(),
                    bundle.primary().flags.app_ack_requested,
                    total_len,
                    &mut payload,
                )
                .await
            }
        };

        if let Err(e) = delivery_result {
            debug!("Service delivery deferred: {e}");
            // Park under the registration EID for the next registration; the
            // park re-checks the routing snapshot, so a service that
            // (re-)registered while this delivery was in flight re-dispatches
            // the bundle instead of stranding it (see park_bundle).
            return OfferOutcome::Parked(
                bundle,
                bundle::BundleStatus::WaitingForService {
                    service: service_eid,
                },
                seen,
            );
        }

        OfferOutcome::Completed(bundle)
    }
}
