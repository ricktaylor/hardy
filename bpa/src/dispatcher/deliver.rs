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
use crate::service::table::{Kind, Record};

impl Dispatcher {
    #[cfg_attr(feature = "instrument", instrument(skip(self, bundle),fields(bundle.id = %bundle.id())))]
    pub(super) async fn deliver_bundle(&self, record: Arc<Record>, bundle: bundle::Bundle) {
        let Some((mut bundle, data)) = self.load_data_or_drop(bundle).await else {
            return;
        };

        // The claim key and every park in the offer use the canonical
        // registration EID stored at construction, the key
        // `poll_service_waiting` matches on re-registration. The bundle's
        // own destination can be a different `Eid` variant for the same
        // endpoint, `LegacyIpn` against `Ipn`, and would never match.
        let service_eid = record.eid().clone();

        // Snapshot the routing table before the claim: the parks in the
        // offer re-check it to close the park-vs-poll window (see
        // park_bundle).
        let seen = self.rib.table_snapshot();

        // Delivery commits at the claim below, where the reaper defers an
        // in-flight delivery, so a bundle that has already expired is
        // resolved here as the reaper would resolve it, rather than
        // offered.
        if bundle.has_expired() {
            return self.drop_bundle(bundle, ReasonCode::LifetimeExpired).await;
        }

        // Claim the bundle out of its delivery queue before offering it.
        // The claim is a conditional swap, because the delivery channel
        // delivers at least once and a duplicate recovered by the storage
        // poller must lose here rather than deliver twice. The new status
        // also marks the point past which the delivery cannot be recalled:
        // the reaper defers it, and the unregister sweep touches only the
        // queued status.
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

        // One expression from claim to resolution: the offer's outcome
        // resolves the claim.
        self.resolve_offer(
            OfferKind::Delivery,
            self.offer_to_service(record, service_eid, bundle, data, seen)
                .await,
        )
        .await
    }

    /// Offers a claimed bundle to its service.
    ///
    /// Runs inside the `DeliveryAckPending` claim: every exit is an
    /// [`OfferOutcome`] the caller resolves, so the claim cannot dangle.
    async fn offer_to_service(
        &self,
        record: Arc<Record>,
        service_eid: Eid,
        mut bundle: bundle::Bundle,
        data: Bytes,
        seen: routing::RibSnapshot,
    ) -> OfferOutcome {
        // The block removals ingress deferred apply first, so the Deliver
        // chain sees the bundle as it is delivered, and the strip can
        // never delete a Rewriter's insert into a removed block's number.
        let data = self.strip_removed_blocks(&mut bundle, data);

        // The Deliver chain strips the transport blocks in its Rewriters,
        // then runs its Verifiers.
        let (bundle, mut data) = match self.filters.run_deliver(bundle, data, &*self.key_provider) {
            filter::ChainOutcome::Continue(bundle, data) => (bundle, data),
            filter::ChainOutcome::Drop(bundle, reason) => {
                return OfferOutcome::Dropped(bundle, reason);
            }
        };

        let delivery_result = match &record.kind {
            Kind::Raw(service) => {
                // A whole-bundle service takes the bundle bytes, and the
                // whole bundle is in hand, so it travels as a single `Final`
                // segment.
                let total_len = data.len() as u64;
                service
                    .deliver(bundle.id(), bundle.expiry(), total_len, &mut data)
                    .await
            }
            Kind::Payload(service) => {
                // A payload-only service takes the extracted, decrypted
                // payload. `KeyProvider` needs a `&Bundle`, and a match
                // expression scopes the parse, so that its `OperationSet`s,
                // which hold `Rc`s and are therefore `!Send`, are dropped at
                // the arm boundary, before any `.await` in this async fn.
                // The parse consumes `data` and returns the authoritative
                // buffer, the streaming path having concatenated its pushes,
                // and the payload becomes an owned `Bytes` before the arm
                // ends, through `slice_ref` and so without a copy where it
                // is not encrypted.
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
                        // TODO: decide what an undecryptable payload
                        // deserves. For now it parks for the next
                        // registration, which may bring usable keys.
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
                        debug!("Received an invalid payload: {e}");

                        // TODO: wrap the damaged bundle in a junk payload
                        // and forward it to a lost-and-found endpoint. For
                        // now it is dropped.

                        return OfferOutcome::Dropped(
                            bundle,
                            Some(ReasonCode::BlockUnintelligible),
                        );
                    }
                    Ok(payload) => payload,
                };

                // As for a whole-bundle service, the whole payload is in
                // hand, so it travels as a single `Final` segment.
                let total_len = payload.len() as u64;
                service
                    .deliver(
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
            // Park under the registration EID for the next registration.
            // The park re-checks the routing snapshot, so a service that
            // registered again while this delivery was in flight
            // re-dispatches the bundle rather than stranding it (see
            // `park_bundle`).
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
