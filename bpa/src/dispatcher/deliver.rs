use hardy_bpv7::{bpsec::key::KeySet, status_report::ReasonCode};

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
        // The Deliver Rewriters (transport-block strip) edit one editor over
        // the record's block index and the loaded bytes, rebuilt once, only
        // if a link edited; with no Rewriters, none is built. The keys are
        // derived once, over the stored bundle, and only when a stage reads
        // through them: the Rewriters and the Verifiers do.
        let mut data = {
            let editing = self.filters.has_deliver_rewriters();
            let keys = (editing || self.filters.has_deliver_verifiers())
                .then(|| filter::output_keys(&bundle.bpv7, &data, &*self.key_provider));
            let no_keys = KeySet::EMPTY;
            let keys = keys.as_deref().unwrap_or(&no_keys);
            let data = if !editing {
                data
            } else {
                let mut editor = Editor::new(&bundle.bpv7, &data);
                let rewritten = self
                    .filters
                    .run_deliver(&mut editor, &bundle.metadata, keys);

                // The bytes were validated at ingress and every edit was
                // checked when it was staged, so a failed rebuild is a BPA
                // or bpv7 bug.
                let rebuilt = rewritten.then(|| {
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

            // The Deliver Verifiers, over the bundle as it will be delivered.
            if let Some(reason) = self.filters.verify_deliver(&bundle, &data, keys) {
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
                // The payload as stored. Its security operations were the
                // destination's to accept at ingress (RFC 9172 §1.4), where
                // this node decrypted it into the store or refused the
                // bundle, so one still under a BCB here is a record no door
                // of this build wrote: dropped, as the gate drops a bundle
                // its destination cannot accept. The record's block index
                // and its bytes were validated at ingress, so a disagreement
                // between them is a BPA bug or storage corruption, and fatal.
                let block = bundle
                    .bpv7
                    .blocks
                    .get(&1)
                    .trace_expect("A stored bundle has no payload block");
                if block.bcb.is_some() {
                    debug!("Payload still encrypted at delivery; dropped");
                    return OfferOutcome::Dropped(bundle, Some(ReasonCode::BlockUnsupported));
                }
                let mut payload = data.slice_ref(
                    block
                        .payload(&data)
                        .trace_expect("A stored bundle's payload extent exceeds its bytes"),
                );

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

#[cfg(test)]
mod tests {
    #[cfg(feature = "rfc9173")]
    use hardy_bpv7::{
        bundle::Id,
        eid::{IpnNodeId, NodeId},
        status_report::AdministrativeRecord,
    };

    use super::*;
    #[cfg(feature = "rfc9173")]
    use crate::{
        bpa::{Bpa, BpaRegistration},
        bundle::Origin,
        dispatcher::tests::encrypted_payload_to,
        node_ids::NodeIds,
        storage::{
            BundleMemStorage, BundleStorage, MetadataMemStorage, MetadataStorage, RecoveryResponse,
            Result as StorageResult,
        },
        stream::{Receiver, Segment, Sender, buffer_stream},
    };

    // Bundle storage that hands each deleted bundle's bytes to a receiver.
    #[cfg(feature = "rfc9173")]
    struct DeletingStore {
        inner: BundleMemStorage,
        deleted_tx: flume::Sender<Bytes>,
    }

    #[cfg(feature = "rfc9173")]
    impl DeletingStore {
        fn new() -> (Arc<Self>, flume::Receiver<Bytes>) {
            let (deleted_tx, deleted_rx) = flume::unbounded();
            (
                Arc::new(Self {
                    inner: BundleMemStorage::new(None, None),
                    deleted_tx,
                }),
                deleted_rx,
            )
        }
    }

    #[cfg(feature = "rfc9173")]
    #[async_trait]
    impl BundleStorage for DeletingStore {
        async fn recover(&self, stream: &dyn Sender<RecoveryResponse>) -> StorageResult<()> {
            self.inner.recover(stream).await
        }

        async fn load(&self, storage_name: &str) -> StorageResult<Option<Bytes>> {
            self.inner.load(storage_name).await
        }

        async fn save(&self, data: Bytes) -> StorageResult<Arc<str>> {
            self.inner.save(data).await
        }

        async fn delete(&self, storage_name: &str) -> StorageResult<()> {
            if let Some(data) = self.inner.load(storage_name).await? {
                let _ = self.deleted_tx.send(data);
            }
            self.inner.delete(storage_name).await
        }
    }

    // An application that hands each delivered payload to a receiver. It
    // keeps its sink: dropping the sink unregisters the application.
    #[cfg(feature = "rfc9173")]
    struct RecordingApp {
        sink: hardy_async::sync::spin::Once<Box<dyn services::ApplicationSink>>,
        delivered_tx: flume::Sender<Bytes>,
    }

    #[cfg(feature = "rfc9173")]
    #[async_trait]
    impl services::Application for RecordingApp {
        async fn on_register(&self, _source: &Eid, sink: Box<dyn services::ApplicationSink>) {
            self.sink.call_once(|| sink);
        }

        async fn on_unregister(&self) {}

        async fn on_deliver(
            &self,
            _bundle_id: &Id,
            _expiry: time::OffsetDateTime,
            _ack_requested: bool,
            total_len: u64,
            stream: &mut dyn Receiver<Segment>,
        ) -> services::Result<()> {
            let payload = buffer_stream(stream, total_len).await?;
            let _ = self.delivered_tx.send(payload);
            Ok(())
        }

        async fn on_status_notify(
            &self,
            _bundle_id: &Id,
            _from: &Eid,
            _kind: services::StatusNotify,
            _reason: ReasonCode,
            _timestamp: Option<time::OffsetDateTime>,
        ) {
        }
    }

    // A stored record whose payload is still under a BCB, one written before
    // the destination accepted at ingress, is not handed to the application
    // as ciphertext and does not wait for a key: it is dropped as the gate
    // drops a bundle its destination cannot accept, "Block unsupported",
    // and the deletion is reported.
    #[cfg(feature = "rfc9173")]
    #[tokio::test]
    async fn a_payload_still_encrypted_at_delivery_is_dropped_unsupported() {
        let (bpv7, data) = encrypted_payload_to("ipn:0.1.9");
        let metadata_store = Arc::new(MetadataMemStorage::new(None));
        let (bundle_store, deleted_rx) = DeletingStore::new();
        let node_ids = NodeIds::try_from(
            [NodeId::Ipn(IpnNodeId {
                allocator_id: 0,
                node_number: 1,
            })]
            .as_slice(),
        )
        .unwrap();
        let bpa = Bpa::builder()
            .node_ids(node_ids)
            .metadata_storage(metadata_store.clone())
            .bundle_storage(bundle_store.clone())
            .status_reports(true)
            .build()
            .await
            .unwrap();
        bpa.start(false).await;

        // The record, parked for service 9 as stored.
        let storage_name = bundle_store.save(data.clone()).await.unwrap();
        let mut record = bundle::Bundle::new(
            bpv7,
            bundle::BundleMetadata::new(time::OffsetDateTime::now_utc(), Origin::Recovered),
        );
        record.metadata.storage_name = Some(storage_name);
        record.status = bundle::BundleStatus::WaitingForService {
            service: "ipn:0.1.9".parse().unwrap(),
        };
        assert!(metadata_store.insert(&record).await.unwrap());

        // Registration polls the park and offers the record.
        let (delivered_tx, delivered_rx) = flume::unbounded();
        bpa.register_application(
            hardy_bpv7::eid::Service::Ipn(9),
            Arc::new(RecordingApp {
                sink: hardy_async::sync::spin::Once::new(),
                delivered_tx,
            }),
        )
        .await
        .unwrap();

        // The timeouts only bound a regression.
        let deleted =
            tokio::time::timeout(core::time::Duration::from_secs(5), deleted_rx.recv_async())
                .await
                .expect("the record is deleted")
                .expect("the store is open");
        assert_eq!(deleted, data, "the encrypted record is deleted");

        // The first delivery is the deletion report, not the ciphertext.
        let delivered = tokio::time::timeout(
            core::time::Duration::from_secs(5),
            delivered_rx.recv_async(),
        )
        .await
        .expect("the deletion is reported to the application")
        .expect("the application is registered");
        let AdministrativeRecord::BundleStatusReport(status) =
            hardy_cbor::decode::parse(&delivered).expect("the first delivery is the report");
        assert!(status.deleted.is_some(), "deletion asserted");
        assert_eq!(status.reason, ReasonCode::BlockUnsupported);
        bpa.shutdown().await;
    }
}
