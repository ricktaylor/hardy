// `Bpv7Error` disambiguates the bpv7 wire-format error from this crate's
// `Error` in scope via the parent module.
use hardy_bpv7::{
    Error as Bpv7Error,
    block::Payload,
    bpsec,
    parse::{self, Parsed},
    status_report::ReasonCode,
};

use super::*;
use crate::services::registry::{Service, ServiceImpl};

impl Dispatcher {
    // Apply the deferred §E block removals to a bundle's wire form for
    // delivery to a raw-bundle service — the deliver-side twin of the egress
    // rewrite head. Returns `data` unchanged when nothing is queued (the
    // common case, one branch and no re-parse); otherwise re-parses, runs the
    // removal cascade with a fresh key source, and flattens the result. The
    // stored bundle is never touched.
    fn strip_removed_blocks(
        &self,
        bundle: &bundle::Bundle,
        data: Bytes,
    ) -> Result<Bytes, hardy_bpv7::editor::Error> {
        use hardy_bpv7::bpsec::edit::BPSecEditor;

        if bundle.metadata.to_remove.is_empty() {
            return Ok(data);
        }
        let Parsed {
            data, bundle: raw, ..
        } = parse::parse(data).map_err(hardy_bpv7::editor::Error::from)?;
        let key_source = self.key_source(&raw, &data);
        let to_remove = bundle.metadata.to_remove.iter().copied().collect();
        let editor = hardy_bpv7::editor::Editor::new(&raw, &data)
            .remove_blocks(to_remove, key_source.as_ref())
            .map_err(|(_, e)| e)?
            .0;
        let (_, chunks) = editor.rebuild_bundle()?;
        Ok(hardy_bpv7::editor::Chunk::flatten_bytes(chunks, data))
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, bundle),fields(bundle.id = %bundle.id())))]
    pub(super) async fn deliver_bundle(&self, service: Arc<Service>, bundle: bundle::Bundle) {
        let Some((mut bundle, data)) = self.load_data_or_drop(bundle).await else {
            return;
        };

        // The claim key and every park below use the canonical registration
        // EID — the exact key `poll_service_waiting` matches on
        // re-registration. The bundle's own destination can be a different
        // Eid variant for the same endpoint (e.g. LegacyIpn vs Ipn) and
        // would never match.
        let service_eid = self
            .node_ids
            .resolve_eid(&service.service_id)
            .unwrap_or_else(|_| bundle.primary().destination.clone());

        // Snapshot the routing table before the claim: the parks below
        // re-check it to close the park-vs-poll window (see park_bundle).
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

        // Every exit below this point must resolve the claim taken above:
        // DeliveryAckPending has no storage poller and the reaper defers it,
        // so a bundle left there is invisible until restart.

        // Deliver chain: Rewriters (transport-block strip), then Verifiers.
        let (bundle, data) = match self.filters.run_deliver(bundle, data, &*self.key_provider) {
            Ok(filter::ChainOutcome::Continue(bundle, data)) => (bundle, data),
            Ok(filter::ChainOutcome::Drop(bundle, Some(reason))) => {
                return self.drop_bundle(bundle, reason).await;
            }
            Ok(filter::ChainOutcome::Drop(bundle, None)) => {
                return self.delete_bundle(bundle).await;
            }
            Err((bundle, e)) => {
                error!("Deliver filter chain failed: {e}");

                // The chain hands the claimed bundle back: park it for the
                // next registration, CAS-clean. Losing the park means a
                // sweep or the reaper resolved it first.
                return self
                    .park_bundle(
                        bundle,
                        bundle::BundleStatus::WaitingForService {
                            service: service_eid,
                        },
                        &seen,
                    )
                    .await;
            }
        };

        let delivery_result = match &service.service {
            ServiceImpl::LowLevel(svc) => {
                // Strip the §E block removals the ingress gate deferred before
                // handing raw bytes to a low-level service — the stored bundle
                // is as received, the removals apply per delivery attempt (the
                // deliver-side twin of the egress rewrite head). A strip
                // failure means the removal cascade cannot be re-applied (a
                // logic bug on a validated bundle, or a key that has since
                // rotated away): drop rather than deliver a bundle that still
                // carries a block §5.1.1 requires gone.
                let mut data = match self.strip_removed_blocks(&bundle, data) {
                    Ok(data) => data,
                    Err(e) => {
                        debug!("Cannot apply deferred block removals at delivery: {e}");
                        return self
                            .drop_bundle(bundle, ReasonCode::BlockUnintelligible)
                            .await;
                    }
                };
                // The whole bundle is in hand, so it travels as a single Final
                // segment.
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
                        match bpsec::block_data(1, &raw.blocks, &buf, &bcb_ops, &*key_source) {
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
                        // bring usable keys) — the claim must not be left
                        // dangling in DeliveryAckPending.
                        debug!("Failed to decrypt payload: No valid keys");
                        return self
                            .park_bundle(
                                bundle,
                                bundle::BundleStatus::WaitingForService {
                                    service: service_eid,
                                },
                                &seen,
                            )
                            .await;
                    }
                    Err(e) => {
                        // Other decryption error - skip delivery
                        debug!("Received an invalid payload: {e}");

                        // TODO: This is where we can wrap the damaged bundle in a "Junk Bundle Payload" and forward it to a 'lost+found' endpoint.  For now we just drop it.

                        return self
                            .drop_bundle(bundle, ReasonCode::BlockUnintelligible)
                            .await;
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
            return self
                .park_bundle(
                    bundle,
                    bundle::BundleStatus::WaitingForService {
                        service: service_eid,
                    },
                    &seen,
                )
                .await;
        }

        // The terminal claim is a conditional tombstone: the reaper races
        // in-flight deliveries, and losing the claim means it resolved the
        // bundle first. Its "Lifetime expired" deletion report has gone
        // out, so this delivery must stay silent rather than contradict it.
        if !self.store.tombstone_if(&bundle).await {
            debug!(
                "Delivery completion for {} lost the resolution race, ignored",
                bundle.id()
            );
            return;
        }

        metrics::counter!("bpa.bundle.delivered").increment(1);
        self.report_bundle_delivery(&bundle).await;

        // Don't use drop_bundle() as we do not want to count the Drop as a 'dropped bundle'
        self.report_bundle_deletion(&bundle, ReasonCode::NoAdditionalInformation)
            .await;
        self.delete_bundle(bundle).await
    }
}
