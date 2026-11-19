use super::{ingress::Received, *};

impl Dispatcher {
    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    pub(crate) async fn restart_bundle(
        &self,
        storage_name: Arc<str>,
        file_time: time::OffsetDateTime,
    ) {
        let Some(mut data) = self.store.load_data(&storage_name).await else {
            // The data went while the restart was running, and the reaper
            // has not started, so this is data loss. Metadata recovery
            // reports it if the bundle is in the metadata store.
            return;
        };

        // Check the stored bundle data for corruption. The parse preserves
        // the bundle rather than running the full ingress pipeline, because
        // ingress processed it already and restart verifies integrity and
        // resumes rather than re-applying block removal or canonicalisation.
        // A missing key is soft here: this is already-accepted data, so a key
        // that has since rotated away must not drop the bundle, and the
        // `nokey_ext` facts are ignored.
        let bundle = match crate::bundle::parse::parse_validate_with_provider(
            data.clone(),
            self.key_provider(),
        ) {
            Ok(bundle) => bundle,
            Err(e) => {
                // No bundle id can be extracted, so the metadata cannot be
                // checked or cleaned up here. Metadata recovery catches any
                // orphan referencing this `storage_name`.
                warn!("Corrupt bundle data found: {storage_name}, {e}");
                self.store.delete_data(&storage_name).await;
                metrics::counter!("bpa.restart.junk").increment(1);
                return;
            }
        };

        if let Some((metadata, status)) = self.store.confirm_exists(&bundle.primary.id).await {
            if metadata.storage_name.as_ref() != Some(&storage_name) {
                // The metadata references another copy, so this one is a
                // duplicate.
                if metadata.storage_name.is_none() {
                    warn!("Duplicate copy of processed bundle data found: {storage_name}");
                } else {
                    warn!(
                        "Duplicate bundle data found: {storage_name} != {:?}",
                        metadata.storage_name.as_ref()
                    );
                }
                self.store.delete_data(&storage_name).await;
                metrics::counter!("bpa.restart.duplicate").increment(1);
                return;
            }

            // Resume from the checkpoint status, pairing the stored record
            // with the freshly parsed bundle: the bytes on disk stay
            // authoritative for the wire half.
            let bundle = bundle::Bundle {
                bpv7: bundle,
                metadata,
                status,
            };
            match &bundle.status {
                // `Dispatching` was claimed by the consumer with the
                // processing never completed, and `DispatchPending` is still
                // queued. Both re-enqueue: the chain has run, so the bundle
                // is re-dispatched rather than run through it again.
                bundle::BundleStatus::Dispatching | bundle::BundleStatus::DispatchPending => {
                    metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&bundle.status)).increment(1.0);
                    self.dispatch_bundle(bundle).await;
                }
                bundle::BundleStatus::ForwardPending { .. }
                | bundle::BundleStatus::ForwardAckPending { .. }
                | bundle::BundleStatus::DeliverPending { .. }
                | bundle::BundleStatus::DeliveryAckPending { .. } => {
                    // Peer ids and CLA registrations are stale after a
                    // restart, so queued bundles re-route and an in-flight
                    // transfer's outcome can never arrive, leaving it
                    // unknown, and the transfer checkpoints reset to
                    // `Waiting`. Service registrations do not survive a
                    // restart either, so queued and in-flight deliveries
                    // re-park as `WaitingForService` and are recovered when
                    // the service registers again. The reset is a
                    // conditional swap from the recovery read: registration
                    // waits on recovery, but a checkpoint that has moved on
                    // regardless is live state this reset must not
                    // overwrite.
                    let parked = match &bundle.status {
                        bundle::BundleStatus::DeliverPending { service }
                        | bundle::BundleStatus::DeliveryAckPending { service } => {
                            bundle::BundleStatus::WaitingForService {
                                service: service.clone(),
                            }
                        }
                        _ => bundle::BundleStatus::Waiting,
                    };
                    let mut bundle = bundle;
                    metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&bundle.status)).increment(1.0);
                    if !self.store.swap_status(&mut bundle, &parked).await {
                        debug!("Recovered checkpoint moved on, leaving it be");
                    }
                }
                // These have their own recovery: `poll_waiting`,
                // `poll_service_waiting` on re-registration, and the
                // fragment reassembly poll. `New` never reaches storage,
                // fresh ingress running the chain in memory and writing a
                // single `Dispatching` checkpoint, so it is not a
                // recoverable state and a row carrying it keeps its gauge.
                // There is no wildcard arm, so that a new status has to
                // choose its re-admission here rather than assume some
                // poller recovers it.
                bundle::BundleStatus::New
                | bundle::BundleStatus::Waiting
                | bundle::BundleStatus::WaitingForService { .. }
                | bundle::BundleStatus::AduFragment { .. } => {
                    metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&bundle.status)).increment(1.0);
                }
            }
        } else {
            // An orphan: the data exists with no metadata. The full receive
            // pipeline runs it, `process_received_bundle` parsing,
            // validating, reporting, running the Ingress filter and
            // executing the routing decision, with the loaded bytes as the
            // bundle stream. The pipeline's spool saves an admitted bundle
            // fresh, so the orphan copy is stranded in every outcome and
            // deleted below. A crash before the delete re-admits it on the
            // next restart, where it loses as a duplicate.
            let metadata = bundle::BundleMetadata::new(file_time, bundle::Origin::Recovered);
            match self.process_received_bundle(&mut data, metadata).await {
                Received::Dispatched | Received::Disposed => {}
                // A stored orphan has no live transfer to refuse, and this
                // is reachable only where the size cap tightened across the
                // restart.
                Received::Refused => {
                    warn!("Restart orphan refused, deleted");
                }
            }
            self.store.delete_data(&storage_name).await;
            metrics::counter!("bpa.restart.orphan").increment(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use hardy_bpv7::{
        bundle::Id,
        eid::{Eid, IpnNodeId, NodeId, Service as ServiceId},
    };

    use super::*;
    use crate::{
        bpa::Bpa,
        cla::{Context, Factory, RegistrarExt, Request, factory_fn},
        service::{RegistrarExt as ServiceRegistrarExt, raw},
        storage::{
            BundleMemStorage, BundleStorage, ConfirmResponse, MetadataMemStorage, MetadataStorage,
            Result as StorageResult,
        },
        stream::Sender,
    };

    struct RecordingCla {
        sink: Arc<dyn cla::Sink>,
        offers_tx: flume::Sender<Id>,
    }

    impl RecordingCla {
        fn factory() -> (impl Factory<Component = Self>, flume::Receiver<Id>) {
            let (offers_tx, offers_rx) = flume::bounded(16);
            (
                factory_fn(Request::default(), move |cx: Context| {
                    Ok(Arc::new(Self {
                        sink: cx.sink,
                        offers_tx,
                    }))
                }),
                offers_rx,
            )
        }
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

    // `MetadataMemStorage` opts out of restart recovery, in-memory metadata
    // being unable to survive a real restart, so its `confirm_exists` is a
    // stub. This wrapper answers `confirm_exists` from the live map, which
    // makes the replay path reachable in a test that seeds the same storage
    // instance it recovers from.
    struct RecoverableMem(MetadataMemStorage);

    #[async_trait]
    impl MetadataStorage for RecoverableMem {
        async fn get(&self, bundle_id: &Id) -> StorageResult<Option<bundle::Bundle>> {
            self.0.get(bundle_id).await
        }

        async fn insert(&self, bundle: &bundle::Bundle) -> StorageResult<bool> {
            self.0.insert(bundle).await
        }

        async fn swap_status(
            &self,
            bundle_id: &Id,
            expected: &bundle::BundleStatus,
            status: &bundle::BundleStatus,
        ) -> StorageResult<bool> {
            self.0.swap_status(bundle_id, expected, status).await
        }

        async fn tombstone_if(
            &self,
            bundle_id: &Id,
            expected: &bundle::BundleStatus,
        ) -> StorageResult<bool> {
            self.0.tombstone_if(bundle_id, expected).await
        }

        async fn tombstone(&self, bundle_id: &Id) -> StorageResult<()> {
            self.0.tombstone(bundle_id).await
        }

        async fn start_recovery(&self) {}

        async fn confirm_exists(&self, bundle_id: &Id) -> StorageResult<Option<ConfirmResponse>> {
            Ok(self
                .0
                .get(bundle_id)
                .await?
                .map(|bundle| (bundle.metadata, bundle.status)))
        }

        async fn remove_unconfirmed(
            &self,
            stream: &dyn Sender<bundle::Bundle>,
        ) -> StorageResult<()> {
            self.0.remove_unconfirmed(stream).await
        }

        async fn reset_peer_queue(&self, peer: u32) -> StorageResult<u64> {
            self.0.reset_peer_queue(peer).await
        }

        async fn reset_peer_ack_pending(&self, peer: u32) -> StorageResult<u64> {
            self.0.reset_peer_ack_pending(peer).await
        }

        async fn reset_service_queue(&self, service: &Eid) -> StorageResult<u64> {
            self.0.reset_service_queue(service).await
        }

        async fn poll_expiry(&self, stream: &dyn Sender<bundle::Bundle>) -> StorageResult<()> {
            self.0.poll_expiry(stream).await
        }

        async fn poll_waiting(&self, stream: &dyn Sender<bundle::Bundle>) -> StorageResult<()> {
            self.0.poll_waiting(stream).await
        }

        async fn poll_service_waiting(
            &self,
            source: Eid,
            stream: &dyn Sender<bundle::Bundle>,
        ) -> StorageResult<()> {
            self.0.poll_service_waiting(source, stream).await
        }

        async fn poll_adu_fragments(
            &self,
            stream: &dyn Sender<bundle::Bundle>,
            status: &bundle::BundleStatus,
        ) -> StorageResult<()> {
            self.0.poll_adu_fragments(stream, status).await
        }

        async fn poll_pending(
            &self,
            stream: &dyn Sender<bundle::Bundle>,
            status: &bundle::BundleStatus,
            limit: usize,
        ) -> StorageResult<()> {
            self.0.poll_pending(stream, status, limit).await
        }
    }

    // A transfer still `ForwardAckPending` when the BPA comes back up has
    // an unknown outcome, which an unclean stop leaves behind, a clean
    // shutdown's unregistration sweep resolving it first. Recovery resets it
    // to `Waiting`, and it is re-offered once a route to its destination
    // reappears.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restart_resets_ack_pending_transfer() {
        let metadata_store = Arc::new(RecoverableMem(MetadataMemStorage::new(None)));
        let data_store = Arc::new(BundleMemStorage::new(None, None));

        // Seed the stores as an unclean stop would leave them: the bundle
        // data present, the metadata parked in `ForwardAckPending` under a
        // peer that is now stale.
        let (_, data) = hardy_bpv7::builder::Builder::new(
            "ipn:0.3.1".parse().unwrap(),
            "ipn:0.2.99".parse().unwrap(),
        )
        .with_payload(b"restart-replay".to_vec().into())
        .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
        .unwrap();
        let data = Bytes::from(data);
        let storage_name = data_store.save(data.clone()).await.unwrap();
        let parsed = crate::bundle::parse::parse_validate_with_provider(
            data.clone(),
            hardy_bpv7::bpsec::no_keys,
        )
        .unwrap();
        let mut metadata = bundle::BundleMetadata::originated();
        metadata.storage_name = Some(storage_name);
        let bundle = bundle::Bundle {
            bpv7: parsed,
            metadata,
            status: bundle::BundleStatus::ForwardAckPending { peer: 7 },
        };
        let id = bundle.id().clone();
        assert!(metadata_store.insert(&bundle).await.unwrap());

        let node_ids = crate::node_ids::NodeIds::try_from(
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
            .bundle_storage(data_store)
            .build();
        bpa.start(true).await.unwrap();

        // `start(true)` awaits recovery, so the reset is visible the moment
        // it returns, with nothing to wait for.
        assert_eq!(
            metadata_store
                .get(&id)
                .await
                .unwrap()
                .expect("Recovered bundle missing from metadata store")
                .status,
            bundle::BundleStatus::Waiting,
            "start(true) resolves only after recovery reset the transfer"
        );

        // A route to the destination appears, and the bundle is
        // re-offered.
        let (factory, offers_rx) = RecordingCla::factory();
        let cla = bpa.register_cla("recording-2", factory).await.unwrap();
        cla.sink
            .add_peer(
                cla::Address::Private("peer-2".as_bytes().into()),
                &[NodeId::Ipn(IpnNodeId {
                    allocator_id: 0,
                    node_number: 2,
                })],
            )
            .await
            .unwrap();

        // the timeout only bounds a regression
        let id2 = tokio::time::timeout(tokio::time::Duration::from_secs(5), offers_rx.recv_async())
            .await
            .expect("Timeout waiting for re-offer")
            .expect("Channel closed");
        assert_eq!(id, id2, "Re-offer must be the recovered bundle");

        bpa.shutdown().await;
    }

    // A bundle still queued for dispatch, at `DispatchPending`, when the
    // BPA comes back up re-enters the dispatch pipeline on recovery, and is
    // offered for forwarding once a route to its destination exists.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restart_requeues_dispatch_pending_bundle() {
        let metadata_store = Arc::new(RecoverableMem(MetadataMemStorage::new(None)));
        let data_store = Arc::new(BundleMemStorage::new(None, None));

        // Seed the stores as an unclean stop would leave them: the bundle
        // data present, the metadata still queued at `DispatchPending`.
        let (_, data) = hardy_bpv7::builder::Builder::new(
            "ipn:0.3.1".parse().unwrap(),
            "ipn:0.2.99".parse().unwrap(),
        )
        .with_payload(b"restart-requeue".to_vec().into())
        .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
        .unwrap();
        let data = Bytes::from(data);
        let storage_name = data_store.save(data.clone()).await.unwrap();
        let parsed = crate::bundle::parse::parse_validate_with_provider(
            data.clone(),
            hardy_bpv7::bpsec::no_keys,
        )
        .unwrap();
        let mut metadata = bundle::BundleMetadata::originated();
        metadata.storage_name = Some(storage_name);
        let bundle = bundle::Bundle {
            bpv7: parsed,
            metadata,
            status: bundle::BundleStatus::DispatchPending,
        };
        let id = bundle.id().clone();
        assert!(metadata_store.insert(&bundle).await.unwrap());

        let node_ids = crate::node_ids::NodeIds::try_from(
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
            .bundle_storage(data_store)
            .build();
        bpa.start(true).await.unwrap();

        // A route to the destination, which the recovered bundle is
        // dispatched to whether recovery ran ahead of the peer, parking at
        // `Waiting` and re-dispatching on the routing event, or found it in
        // place.
        let (factory, offers_rx) = RecordingCla::factory();
        let cla = bpa.register_cla("recording-3", factory).await.unwrap();
        cla.sink
            .add_peer(
                cla::Address::Private("peer-3".as_bytes().into()),
                &[NodeId::Ipn(IpnNodeId {
                    allocator_id: 0,
                    node_number: 2,
                })],
            )
            .await
            .unwrap();

        // The timeout only bounds a regression.
        let id2 = tokio::time::timeout(tokio::time::Duration::from_secs(5), offers_rx.recv_async())
            .await
            .expect("Timeout waiting for the recovered bundle's offer")
            .expect("Channel closed");
        assert_eq!(id, id2, "Offer must be the recovered bundle");

        bpa.shutdown().await;
    }

    // A local delivery service that drains each delivery and records the
    // bundle id.
    struct CapturingService {
        delivered_tx: flume::Sender<Id>,
    }

    impl CapturingService {
        fn factory(delivered_tx: flume::Sender<Id>) -> impl raw::Factory<Component = Self> {
            raw::factory_fn(move |_| Ok(Arc::new(Self { delivered_tx })))
        }
    }

    #[async_trait]
    impl raw::Service for CapturingService {
        async fn deliver(
            &self,
            bundle_id: &Id,
            _expiry: time::OffsetDateTime,
            _total_len: u64,
            stream: &mut dyn crate::stream::Receiver<crate::stream::Segment>,
        ) -> crate::service::Result<()> {
            loop {
                match stream.recv().await {
                    Ok(crate::stream::Segment::Final(_)) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            let _ = self.delivered_tx.send_async(bundle_id.clone()).await;
            Ok(())
        }

        async fn report_status(
            &self,
            _bundle_id: &Id,
            _from: &Eid,
            _kind: crate::service::StatusNotify,
            _reason: hardy_bpv7::status_report::ReasonCode,
            _timestamp: Option<time::OffsetDateTime>,
        ) {
        }
    }

    // Channel statuses never outlive their channel: a delivery found at
    // restart queued at `DeliverPending` or in flight at
    // `DeliveryAckPending` is re-parked as `WaitingForService`, and the
    // service's registration delivers both.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restart_reparks_delivery_statuses() {
        let metadata_store = Arc::new(RecoverableMem(MetadataMemStorage::new(None)));
        let data_store = Arc::new(BundleMemStorage::new(None, None));

        // Seed the stores as an unclean stop would leave them: one bundle
        // still queued in its delivery channel, one mid-delivery.
        let service_eid: Eid = "ipn:0.1.7".parse().unwrap();
        let mut ids = Vec::new();
        for (i, status) in [
            bundle::BundleStatus::DeliverPending {
                service: service_eid.clone(),
            },
            bundle::BundleStatus::DeliveryAckPending {
                service: service_eid.clone(),
            },
        ]
        .into_iter()
        .enumerate()
        {
            let (_, data) = hardy_bpv7::builder::Builder::new(
                format!("ipn:0.3.{}", i + 1).parse().unwrap(),
                "ipn:0.1.7".parse().unwrap(),
            )
            .with_payload(b"restart-redeliver".to_vec().into())
            .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
            .unwrap();
            let data = Bytes::from(data);
            let storage_name = data_store.save(data.clone()).await.unwrap();
            let parsed = crate::bundle::parse::parse_validate_with_provider(
                data.clone(),
                hardy_bpv7::bpsec::no_keys,
            )
            .unwrap();
            let mut metadata = bundle::BundleMetadata::originated();
            metadata.storage_name = Some(storage_name);
            let bundle = bundle::Bundle {
                bpv7: parsed,
                metadata,
                status,
            };
            ids.push(bundle.id().clone());
            assert!(metadata_store.insert(&bundle).await.unwrap());
        }

        let node_ids = crate::node_ids::NodeIds::try_from(
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
            .bundle_storage(data_store)
            .build();
        bpa.start(true).await.unwrap();

        // `start(true)` awaits recovery, so both re-parks are visible the
        // moment it returns, with nothing to wait for.
        for id in &ids {
            assert_eq!(
                metadata_store
                    .get(id)
                    .await
                    .unwrap()
                    .expect("Recovered bundle missing from metadata store")
                    .status,
                bundle::BundleStatus::WaitingForService {
                    service: service_eid.clone(),
                },
                "start(true) resolves only after recovery re-parked the delivery"
            );
        }

        // The service registers, and its `WaitingForService` poll delivers
        // both.
        let (delivered_tx, delivered_rx) = flume::bounded(16);
        bpa.register_raw_service(
            Some(ServiceId::Ipn(7)),
            CapturingService::factory(delivered_tx),
        )
        .await
        .unwrap();

        // The timeout only bounds a regression.
        let mut got = Vec::new();
        for _ in 0..ids.len() {
            got.push(
                tokio::time::timeout(
                    tokio::time::Duration::from_secs(5),
                    delivered_rx.recv_async(),
                )
                .await
                .expect("Timeout waiting for the recovered bundle's delivery")
                .expect("Channel closed"),
            );
        }
        got.sort();
        ids.sort();
        assert_eq!(got, ids, "Both re-parked bundles must be delivered");

        bpa.shutdown().await;
    }
}
