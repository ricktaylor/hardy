use hardy_bpv7::{editor::Chunk, status_report::ReasonCode};

use super::{
    output::{LoadedBundle, PayloadReceiver, pull_headers},
    *,
};
use crate::{
    filter::Verdict,
    keys::derive_keys,
    rebuilt_headers::RebuiltHeaders,
    services::registry::{Service, ServiceImpl},
};

impl Dispatcher {
    #[cfg_attr(feature = "instrument", instrument(skip(self, bundle),fields(bundle.id = %bundle.id())))]
    pub(super) async fn deliver_bundle(&self, service: Arc<Service>, mut bundle: bundle::Bundle) {
        // A registration's consumer still drains the copies buffered in its
        // queue once the service has begun unregistering (cancellation is
        // its first step). The unregister sweep resolves the queued bundles
        // themselves, and the same endpoint's next registration may already
        // have queued one again under the same status, so a copy claimed
        // here would take that bundle from it: act on none of them.
        if service.cancel.is_cancelled() {
            debug!("Service unregistering, leaving the queued copy to the sweep");
            return;
        }

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

        // Delivery commits at the claim below — the reaper defers a claimed
        // delivery — so never claim a bundle that has already expired:
        // resolve it as the reaper would.
        if bundle.has_expired() {
            return self.drop_bundle(bundle, ReasonCode::LifetimeExpired).await;
        }

        if !self.claim_delivery(&mut bundle, &service_eid).await {
            debug!("Bundle already claimed for delivery or swept, skipping offer");
            return;
        }

        // Every exit from here resolves the claim: a load that finds the
        // data missing, or the bundle expired meanwhile, drops it, and the
        // offer's outcome is the claim's resolution.
        let Some((bundle, loaded)) = self
            .load_claimed_delivery(&service, &service_eid, bundle, &seen)
            .await
        else {
            return;
        };
        self.resolve_offer(
            OfferKind::Delivery,
            self.offer_to_service(service, service_eid, bundle, loaded, seen)
                .await,
        )
        .await
    }

    // Claim the bundle out of its delivery queue, before loading its data
    // and offering it. `false` when another copy or a sweep has it.
    //
    // The claim must be a conditional swap: the delivery channel is
    // at-least-once, so a duplicate copy recovered by the storage poller
    // must lose here rather than produce a second delivery, and it loses
    // before the load, so a duplicate costs one metadata compare-and-swap,
    // never a read of the stored bundle. The new status also marks the point
    // past which the delivery cannot be recalled: the reaper defers it, and
    // the unregister sweep only touches the queued status.
    async fn claim_delivery(&self, bundle: &mut bundle::Bundle, service_eid: &Eid) -> bool {
        let claimed = bundle::BundleStatus::DeliveryAckPending {
            service: service_eid.clone(),
        };
        self.store.swap_status(bundle, &claimed).await
    }

    // Load a claimed bundle until its headers are resident, what the service
    // receives streaming on from the store, and confirm the service is still
    // registered. `None` when the load dropped the bundle or the service is
    // going, the bundle then parked for its next registration.
    //
    // The unregister sweep moves only queued deliveries, never this claim,
    // so a service that began unregistering while the headers loaded shows
    // only through its registration's cancellation, checked here. Park the
    // bundle for the next registration, as a delivery the departing service
    // refused would be, rather than offer it to a service that is going: an
    // in-process service could see on_deliver after on_unregister. This
    // leaves only the window between the claim and on_deliver that the claim
    // has always had, and does not close it.
    async fn load_claimed_delivery(
        &self,
        service: &Service,
        service_eid: &Eid,
        bundle: bundle::Bundle,
        seen: &routing::RibSnapshot,
    ) -> Option<(bundle::Bundle, LoadedBundle)> {
        let (bundle, stream) = self.load_or_drop(bundle).await?;
        let loaded = pull_headers(&bundle.bpv7, stream, self.filters.deliver.peek).await;
        if service.cancel.is_cancelled() {
            debug!("Service unregistered during the load, parking the bundle");
            let parked = bundle::BundleStatus::WaitingForService {
                service: service_eid.clone(),
            };
            self.park_bundle(bundle, parked, seen).await;
            return None;
        }
        Some((bundle, loaded))
    }

    /// Offer a claimed bundle to its service. Runs strictly inside the
    /// `DeliveryAckPending` claim: every exit is an [`OfferOutcome`] the
    /// caller resolves, so the claim cannot dangle. The record keeps the
    /// stored bundle's block index throughout: a rebuild is the attempt's
    /// alone.
    async fn offer_to_service(
        &self,
        service: Arc<Service>,
        service_eid: Eid,
        bundle: bundle::Bundle,
        mut loaded: LoadedBundle,
        seen: routing::RibSnapshot,
    ) -> OfferOutcome {
        let delivery_result = match &service.service {
            ServiceImpl::LowLevel(svc) => {
                // A raw service receives the bundle: the rebuilt header region
                // when a Deliver Rewriter edited, else the stored bundle as
                // loaded, the payload streaming on from the store either way.
                let chunks = match self.run_deliver_stages(&bundle, &loaded.resident) {
                    Ok(chunks) => chunks,
                    Err(reason) => return OfferOutcome::Dropped(bundle, reason),
                };
                match chunks {
                    Some(chunks) => {
                        let head = loaded.resident.clone();
                        let mut stream = RebuiltHeaders::new(chunks, &head, &mut loaded);
                        let total_len = stream.total_len(bundle.bpv7.encoded_len());
                        svc.on_deliver(bundle.id(), bundle.expiry(), total_len, &mut stream)
                            .await
                    }
                    None => {
                        let total_len = bundle.bpv7.encoded_len();
                        svc.on_deliver(bundle.id(), bundle.expiry(), total_len, &mut loaded)
                            .await
                    }
                }
            }
            ServiceImpl::Application(app) => {
                // An Application receives the payload alone, streaming from
                // the store as it is read. The Deliver stages run on the held
                // headers and peek for their verdict: a Rewriter edits
                // neither the payload nor its protection.
                if let Err(reason) = self.run_deliver_stages(&bundle, &loaded.resident) {
                    return OfferOutcome::Dropped(bundle, reason);
                }

                // The payload as stored. Its security operations were the
                // destination's to accept at ingress (RFC 9172 §1.4), where
                // this node decrypted it into the store or refused the
                // bundle, so one still under a BCB here is a record no door
                // of this build wrote: dropped, as the gate drops a bundle
                // its destination cannot accept.
                let block = bundle
                    .bpv7
                    .blocks
                    .get(&1)
                    .trace_expect("A stored bundle has no payload block");
                if block.bcb.is_some() {
                    debug!("Payload still encrypted at delivery; dropped");
                    return OfferOutcome::Dropped(bundle, Some(ReasonCode::BlockUnsupported));
                }

                // Announced at its stored data length, the ADU's, known
                // before the first byte.
                let mut stream = PayloadReceiver::new(&bundle.bpv7, loaded);
                let total_len = stream.total_len();
                app.on_deliver(
                    bundle.id(),
                    bundle.expiry(),
                    bundle.primary().flags.app_ack_requested,
                    total_len,
                    &mut stream,
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

    // The Deliver stages on one editor over the record's block index and
    // `data`, the stored bundle's resident bytes: the Rewriters, then the
    // Verifiers over the editor's edits, so nothing is materialised for them.
    // With no Rewriters and no Verifiers no editor is built, and the keys are
    // derived only when a stage reads through them. `Err` carries the
    // dropping Verifier's reason; `Ok` carries the rebuild's chunks when a
    // Rewriter edited. The bytes were validated at ingress and every edit was
    // checked when it was staged, so a failed rebuild is a BPA or bpv7 bug,
    // and fatal.
    fn run_deliver_stages(
        &self,
        bundle: &bundle::Bundle,
        data: &[u8],
    ) -> Result<Option<Vec<Chunk>>, Option<ReasonCode>> {
        if !self.filters.has_deliver_rewriters() && !self.filters.has_deliver_verifiers() {
            return Ok(None);
        }
        let keys = derive_keys(&bundle.bpv7, data, &*self.key_provider);
        let mut editor = Editor::new(&bundle.bpv7, data);
        self.filters
            .rewrite_deliver(&mut editor, &bundle.metadata, &*keys);
        if let Verdict::Drop(reason) =
            self.filters
                .verify_deliver(&editor, data, &bundle.metadata, &*keys)
        {
            return Err(reason);
        }
        Ok(editor.is_modified().then(|| {
            editor
                .rebuild()
                .trace_expect("The delivery rewrite failed to rebuild the bundle")
        }))
    }
}

#[cfg(test)]
mod tests {
    // `ServiceId` keeps bpv7's service identifier apart from the registry's
    // `Service` imported above; `services::Service`, the trait, stays
    // qualified for the same reason.
    use hardy_bpv7::{bundle::Id, eid::Service as ServiceId};
    #[cfg(feature = "rfc9173")]
    use hardy_bpv7::{
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
        storage::{BundleMemStorage, RecoveryResponse, Result as StorageResult},
        stream::{Sender, buffer_stream},
    };
    use crate::{
        dispatcher::tests::{
            CountingBundleStorage, bundle_data, deletion_reasons, reporting_dispatcher, seed,
        },
        lanes::Lanes,
        storage::{BundleStorage, MetadataMemStorage, MetadataStorage},
        stream::{Receiver, Segment},
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
            Lanes::Serial,
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

    const SERVICE: &str = "ipn:0.1.7";

    // Records each delivery, and completes it.
    struct RecordingService {
        deliveries_tx: flume::Sender<Id>,
    }

    #[async_trait]
    impl services::Service for RecordingService {
        async fn on_register(&self, _endpoint: &Eid, _sink: Box<dyn services::ServiceSink>) {}

        async fn on_unregister(&self) {}

        async fn on_deliver(
            &self,
            bundle_id: &Id,
            _expiry: time::OffsetDateTime,
            _total_len: u64,
            _stream: &mut dyn Receiver<Segment>,
        ) -> services::Result<()> {
            let _ = self.deliveries_tx.send(bundle_id.clone());
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

    // The registration `deliver_bundle` delivers to, with its live delivery
    // queue, recording each delivery.
    fn recording_service(dispatcher: &Dispatcher) -> (Arc<Service>, flume::Receiver<Id>) {
        let (deliveries_tx, deliveries_rx) = flume::bounded(16);
        let eid: Eid = SERVICE.parse().unwrap();
        let (tx, _rx) = dispatcher.new_delivery_channel(&eid);
        let service = Service::new(
            ServiceImpl::LowLevel(Arc::new(RecordingService { deliveries_tx })),
            ServiceId::Ipn(7),
            eid,
            tx,
            Lanes::Serial,
        );
        (service, deliveries_rx)
    }

    fn queued() -> bundle::BundleStatus {
        bundle::BundleStatus::DeliverPending {
            service: SERVICE.parse().unwrap(),
        }
    }

    // deliver_bundle never commences a delivery for an expired bundle: the
    // pre-claim expiry checkpoint resolves it as LifetimeExpired before its
    // data is loaded or the service sees it.
    #[tokio::test]
    async fn an_expired_queued_bundle_is_never_loaded() {
        let metadata_store = Arc::new(MetadataMemStorage::new(None));
        let data_store = CountingBundleStorage::new();
        let dispatcher = reporting_dispatcher(metadata_store.clone(), data_store.clone()).await;
        let (service, deliveries_rx) = recording_service(&dispatcher);
        let bundle = seed(
            &metadata_store,
            &*data_store,
            bundle_data(SERVICE, true),
            queued(),
        )
        .await;

        dispatcher.deliver_bundle(service, bundle).await;

        assert!(deliveries_rx.try_recv().is_err(), "nothing is delivered");
        assert_eq!(data_store.loads(), 0, "an expired bundle is never loaded");
        assert_eq!(
            deletion_reasons(&metadata_store, &*data_store).await,
            [ReasonCode::LifetimeExpired]
        );
    }

    // A departing registration's consumer still drains its buffered copies
    // after the service begins unregistering. The bundle such a copy names
    // may already be queued again, under the same status, for the
    // endpoint's next registration: the copy must leave it there, unclaimed
    // and unloaded, not take it to the departing service.
    #[tokio::test]
    async fn a_departing_service_leaves_its_queued_copies() {
        let metadata_store = Arc::new(MetadataMemStorage::new(None));
        let data_store = CountingBundleStorage::new();
        let dispatcher = reporting_dispatcher(metadata_store.clone(), data_store.clone()).await;
        let (service, deliveries_rx) = recording_service(&dispatcher);
        let bundle = seed(
            &metadata_store,
            &*data_store,
            bundle_data(SERVICE, false),
            queued(),
        )
        .await;
        let bundle_id = bundle.id().clone();
        service.cancel.cancel();

        dispatcher.deliver_bundle(service, bundle).await;

        assert!(deliveries_rx.try_recv().is_err(), "nothing is delivered");
        assert_eq!(data_store.loads(), 0, "the copy is never loaded");
        assert_eq!(
            metadata_store
                .get(&bundle_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            queued(),
            "the bundle stays queued for the next registration"
        );
    }

    // The delivery channel delivers at-least-once, so a duplicate copy of
    // a queued bundle can reach `deliver_bundle` after the first copy has
    // been delivered. The duplicate loses the claim before touching bundle
    // storage: one load, one delivery.
    #[tokio::test]
    async fn a_duplicate_copy_loses_the_claim_without_loading() {
        let metadata_store = Arc::new(MetadataMemStorage::new(None));
        let data_store = CountingBundleStorage::new();
        let dispatcher = reporting_dispatcher(metadata_store.clone(), data_store.clone()).await;
        let (service, deliveries_rx) = recording_service(&dispatcher);
        let bundle = seed(
            &metadata_store,
            &*data_store,
            bundle_data(SERVICE, false),
            queued(),
        )
        .await;
        let duplicate = bundle.clone();
        let bundle_id = bundle.id().clone();

        dispatcher.deliver_bundle(service.clone(), bundle).await;
        assert_eq!(deliveries_rx.try_recv().unwrap(), bundle_id);
        assert_eq!(data_store.loads(), 1);

        dispatcher.deliver_bundle(service, duplicate).await;
        assert_eq!(data_store.loads(), 1, "the duplicate never loads");
        assert!(
            deliveries_rx.try_recv().is_err(),
            "the duplicate is not delivered"
        );
        assert_eq!(
            deletion_reasons(&metadata_store, &*data_store).await,
            [ReasonCode::NoAdditionalInformation],
            "only the delivery resolves the bundle"
        );
    }

    // A queued bundle whose data has gone is claimed, found missing at the
    // load, and dropped with `DepletedStorage` from its claim, so the
    // claim — which the reaper defers — never dangles.
    #[tokio::test]
    async fn a_claimed_bundle_whose_data_is_missing_is_dropped() {
        let metadata_store = Arc::new(MetadataMemStorage::new(None));
        let data_store = CountingBundleStorage::new();
        let dispatcher = reporting_dispatcher(metadata_store.clone(), data_store.clone()).await;
        let (service, deliveries_rx) = recording_service(&dispatcher);
        let bundle = seed(
            &metadata_store,
            &*data_store,
            bundle_data(SERVICE, false),
            queued(),
        )
        .await;
        let bundle_id = bundle.id().clone();
        data_store
            .delete(bundle.metadata.storage_name.as_ref().unwrap())
            .await
            .unwrap();

        dispatcher.deliver_bundle(service, bundle).await;

        assert!(deliveries_rx.try_recv().is_err(), "nothing is delivered");
        assert!(
            metadata_store.get(&bundle_id).await.unwrap().is_none(),
            "the claim is resolved"
        );
        assert_eq!(
            deletion_reasons(&metadata_store, &*data_store).await,
            [ReasonCode::DepletedStorage]
        );
    }
}
