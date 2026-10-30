// `Bpv7Error` disambiguates the bpv7 wire-format error from this crate's
// `Error` in scope via the parent module.
use hardy_bpv7::{
    Bundle as Bpv7Bundle, Error as Bpv7Error,
    bpsec::{
        self,
        key::{KeySet, KeySource},
    },
    editor::{Chunk, Editor},
};

use super::{
    output::{ChunkReceiver, PayloadReceiver, Resident, pull_headers, pull_whole},
    *,
};
use crate::services::registry::{Service, ServiceImpl};

impl Dispatcher {
    #[cfg_attr(feature = "instrument", instrument(skip(self, bundle),fields(bundle.id = %bundle.id())))]
    pub(super) async fn deliver_bundle(&self, service: Arc<Service>, bundle: bundle::Bundle) {
        // Open the stored bundle and hold it until its headers are resident:
        // what the service receives streams on from the store.
        let Some((mut bundle, stream)) = self.load_stream_or_drop(bundle).await else {
            return;
        };
        let resident = pull_headers(&bundle.bpv7, stream, self.filters.deliver.peek).await;

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
            self.offer_to_service(service, service_eid, bundle, resident, seen)
                .await,
        )
        .await
    }

    /// Offer a claimed bundle to its service. Runs strictly inside the
    /// `DeliveryAckPending` claim: every exit is an [`OfferOutcome`] the
    /// caller resolves, so the claim cannot dangle.
    ///
    /// The record keeps the stored bundle's block index throughout: a
    /// rebuild is the attempt's alone. The key source the stages read
    /// through is `!Send`, so each arm derives it inside a block that ends
    /// ahead of the service's await.
    async fn offer_to_service(
        &self,
        service: Arc<Service>,
        service_eid: Eid,
        bundle: bundle::Bundle,
        resident: Resident,
        seen: routing::RibSnapshot,
    ) -> OfferOutcome {
        let delivery_result = match &service.service {
            ServiceImpl::LowLevel(svc) => {
                // A raw service receives the bundle: the rebuild's chunks when
                // a stage edited, else the stored bundle, the payload
                // streaming on from the store either way.
                let rebuilt = {
                    let keys = self.delivery_keys(&bundle, &resident.bytes, false);
                    let no_keys = KeySet::EMPTY;
                    let keys = keys.as_deref().unwrap_or(&no_keys);
                    match self.run_deliver_stages(&bundle, &resident.bytes, keys, true) {
                        Ok(rebuilt) => rebuilt,
                        Err(reason) => return OfferOutcome::Dropped(bundle, reason),
                    }
                };
                let (total_len, mut stream) = match rebuilt {
                    Some((rebuilt, chunks)) => {
                        let total_len = rebuilt.encoded_len();
                        (total_len, ChunkReceiver::new(chunks, resident, total_len))
                    }
                    None => {
                        let total_len = bundle.bpv7.encoded_len();
                        (total_len, ChunkReceiver::stored(resident, total_len))
                    }
                };
                svc.on_deliver(bundle.id(), bundle.expiry(), total_len, &mut stream)
                    .await
            }
            ServiceImpl::Application(app) => {
                // An Application receives the payload alone, announced at the
                // length it will receive. The stages run for their verdict:
                // the removals and the Rewriters touch neither the payload
                // nor its protection, so the payload comes from the stored
                // bundle.
                let (total_len, mut stream): (u64, Box<dyn Receiver<Segment>>) =
                    if payload_is_encrypted(&bundle.bpv7) {
                        // Decrypted whole by this node as the payload's
                        // security acceptor (RFC 9172 §5.1.1), so the whole
                        // stored bundle is pulled, and the payload is
                        // announced at its decrypted length: a ciphertext that
                        // carries its authentication tag (RFC 9173 §4.4) is 16
                        // bytes longer than the plaintext. With no key, park
                        // for the next registration, which may bring usable
                        // keys; a payload that fails to decrypt discards the
                        // bundle.
                        let data = pull_whole(resident).await;
                        let payload = {
                            let keys = self.delivery_keys(&bundle, &data, true);
                            let no_keys = KeySet::EMPTY;
                            let keys = keys.as_deref().unwrap_or(&no_keys);
                            if let Err(reason) =
                                self.run_deliver_stages(&bundle, &data, keys, false)
                            {
                                return OfferOutcome::Dropped(bundle, reason);
                            }
                            match payload_bytes(&bundle.bpv7, &data, keys) {
                                Ok(payload) => payload,
                                Err(Bpv7Error::InvalidBPSec(bpsec::Error::NoKey)) => {
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
                                    debug!("Failed to decrypt payload: {e}");

                                    // TODO: This is where we can wrap the damaged bundle in a "Junk Bundle Payload" and forward it to a 'lost+found' endpoint.  For now we just drop it.

                                    return OfferOutcome::Dropped(
                                        bundle,
                                        Some(payload_failure_reason(&e)),
                                    );
                                }
                            }
                        };
                        (payload.len() as u64, Box::new(payload))
                    } else {
                        // A plaintext payload streams from the store as it is
                        // read, announced at its stored data length, the
                        // ADU's, known before the first byte: the stages run
                        // on the held headers and peek.
                        {
                            let keys = self.delivery_keys(&bundle, &resident.bytes, false);
                            let no_keys = KeySet::EMPTY;
                            let keys = keys.as_deref().unwrap_or(&no_keys);
                            if let Err(reason) =
                                self.run_deliver_stages(&bundle, &resident.bytes, keys, false)
                            {
                                return OfferOutcome::Dropped(bundle, reason);
                            }
                        }
                        let stream = PayloadReceiver::new(&bundle.bpv7, resident);
                        (stream.total_len(), Box::new(stream))
                    };
                app.on_deliver(
                    bundle.id(),
                    bundle.expiry(),
                    bundle.primary().flags.app_ack_requested,
                    total_len,
                    &mut *stream,
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

    // The key source for one delivery attempt, derived once over the stored
    // bundle, and only when a stage reads through it: the scheduled
    // removals, the Deliver Rewriters and Verifiers, and, when `decrypting`,
    // an Application's payload decryption. `None` when nothing does.
    fn delivery_keys(
        &self,
        bundle: &bundle::Bundle,
        data: &[u8],
        decrypting: bool,
    ) -> Option<Box<dyn KeySource>> {
        let reads = decrypting
            || !bundle.metadata.to_remove.is_empty()
            || self.filters.has_deliver_rewriters()
            || self.filters.has_deliver_verifiers();
        reads.then(|| filter::output_keys(&bundle.bpv7, data, &*self.key_provider))
    }

    // The Deliver stages on one editor over the record's block index and the
    // resident bytes: the §E removals the ingress gate deferred, then the
    // Deliver Rewriters, then the Deliver Verifiers over a snapshot of the
    // edits, so nothing is materialised for them. With nothing scheduled, no
    // Rewriters and no Verifiers, no editor is built. `Err` carries the
    // dropping Verifier's reason. When `rebuild` and a stage edited, `Ok`
    // carries the one rebuild, whose bytes were validated at ingress and
    // whose every edit was checked when it was staged, so a failure is a BPA
    // or bpv7 bug, and fatal.
    fn run_deliver_stages(
        &self,
        bundle: &bundle::Bundle,
        data: &[u8],
        keys: &dyn KeySource,
        rebuild: bool,
    ) -> Result<Option<(Bpv7Bundle, Vec<Chunk>)>, Option<ReasonCode>> {
        let editing = !bundle.metadata.to_remove.is_empty() || self.filters.has_deliver_rewriters();
        if !editing && !self.filters.has_deliver_verifiers() {
            return Ok(None);
        }
        let editor = Editor::new(&bundle.bpv7, data);

        // The removals apply first, so the Deliver chain sees the bundle as
        // it will be delivered, and the strip can never delete a Rewriter's
        // insert into a removed block's number.
        let (mut editor, stripped) = self.strip_removed_blocks(bundle, editor, keys);

        // Deliver chain, first stage: the Rewriters.
        let rewritten = self
            .filters
            .run_deliver(&mut editor, &bundle.metadata, keys);

        // Deliver chain, second stage: the Verifiers, over the bundle as it
        // will be delivered.
        if let Some(reason) = self
            .filters
            .verify_deliver(&editor, data, &bundle.metadata, keys)
        {
            return Err(reason);
        }

        Ok((rebuild && (stripped || rewritten)).then(|| {
            editor
                .rebuild_bundle()
                .trace_expect("The delivery rewrite failed to rebuild the bundle")
        }))
    }
}
