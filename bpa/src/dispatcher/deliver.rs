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
    #[cfg_attr(feature = "instrument", instrument(skip(self, bundle),fields(bundle.id = %bundle.id())))]
    pub(super) async fn deliver_bundle(
        &self,
        service: Arc<Service>,
        bundle: bundle::Bundle,
    ) {
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
        bundle: bundle::Bundle,
        data: Bytes,
        seen: routing::RibSnapshot,
    ) -> OfferOutcome {
        let bundle_id = bundle.id().clone();

        // Deliver filter hook
        let (bundle, mut data) = match self
            .filter_engine
            .exec(filter::Hook::Deliver, bundle, data, self.key_provider())
            .await
        {
            Ok(filter::ExecResult::Continue(_, bundle, data)) => (bundle, data),
            Ok(filter::ExecResult::Drop(bundle, reason)) => {
                return OfferOutcome::Dropped(bundle, reason);
            }
            Err(e) => {
                error!("Deliver filter execution failed: {e}");

                // The filter consumed the claimed bundle, so re-fetch it and
                // conditionally park it for the next registration. A
                // re-fetch that finds the bundle moved on means a sweep or
                // the reaper resolved it first.
                return match self.store.get_metadata(&bundle_id).await {
                    Some(bundle)
                        if bundle.status
                            == (bundle::BundleStatus::DeliveryAckPending {
                                service: service_eid.clone(),
                            }) =>
                    {
                        OfferOutcome::Parked(
                            bundle,
                            bundle::BundleStatus::WaitingForService {
                                service: service_eid,
                            },
                            seen,
                        )
                    }
                    _ => OfferOutcome::Lost,
                };
            }
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
                        .block_data(1)
                        .and_then(|p| p.ok_or(hardy_bpv7::Error::Altered))
                        {
                            Ok(Payload::Borrowed(s)) => Ok(buf.slice_ref(s)),
                            Ok(Payload::Decrypted(d)) => {
                                Ok(Bytes::from_owner(d))
                            }
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
