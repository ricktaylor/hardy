use super::*;

impl Dispatcher {
    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    pub(crate) async fn restart_bundle(
        &self,
        storage_name: Arc<str>,
        file_time: time::OffsetDateTime,
    ) {
        let Some(data) = self.store.load_data(&storage_name).await else {
            // Data has gone while we were restarting — the reaper hasn't started,
            // so this is data loss. Safe because metadata recovery will report it
            // if the bundle is in the metadata store.
            return;
        };

        // Validate the stored bundle data is not corrupt. We use ParsedBundle
        // (Preserve mode) rather than RewrittenBundle because the bundle was
        // already fully processed at ingress — restart should verify integrity
        // and resume, not re-apply block removal or canonicalization.
        let bundle = match hardy_bpv7::bundle::ParsedBundle::parse(&data, self.key_provider()) {
            Ok(parsed) => parsed.bundle,
            Err(e) => {
                // Can't extract a bundle ID, so we can't check or clean up
                // metadata here. Any orphaned metadata referencing this
                // storage_name will be caught by metadata_storage_recovery.
                warn!("Corrupt bundle data found: {storage_name}, {e}");
                self.store.delete_data(&storage_name).await;
                metrics::counter!("bpa.restart.junk").increment(1);
                return;
            }
        };

        // Reconcile with metadata store
        if let Some(metadata) = self.store.confirm_exists(&bundle.id).await {
            if metadata.storage_name.as_ref() != Some(&storage_name) {
                // Metadata references a different copy — this one is a duplicate
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

            // Resume processing based on checkpoint status
            let bundle = bundle::Bundle { metadata, bundle };
            match &bundle.metadata.status {
                bundle::BundleStatus::New => {
                    // Ingress filter not yet complete — run full ingress
                    self.ingress_bundle(bundle, data).await;
                }
                bundle::BundleStatus::Dispatching => {
                    // Ingress filter done — enqueue for routing
                    metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&bundle.metadata.status)).increment(1.0);
                    self.dispatch_bundle(bundle).await;
                }
                bundle::BundleStatus::ForwardPending { .. }
                | bundle::BundleStatus::ForwardAckPending { .. } => {
                    // Peer IDs and CLA registrations are stale after restart —
                    // queued bundles re-route, and an in-flight transfer's
                    // outcome can never arrive (outcome-unknown) — reset to
                    // Waiting
                    let mut bundle = bundle;
                    metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&bundle.metadata.status)).increment(1.0);
                    self.store
                        .update_status(&mut bundle, &bundle::BundleStatus::Waiting)
                        .await;
                }
                // Other statuses are handled by their respective recovery mechanisms:
                // - Waiting: poll_waiting recovery
                // - WaitingForService: poll_service_waiting on service re-registration
                // - AduFragment: fragment reassembly polling
                _ => {
                    metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&bundle.metadata.status)).increment(1.0);
                }
            }
        } else {
            // Orphan — data exists but no metadata. Run the full receive
            // pipeline (RewrittenBundle parse, block removal, canonicalization,
            // storage, reporting, and Ingress filter).
            let metadata = bundle::BundleMetadata {
                status: bundle::BundleStatus::New,
                storage_name: Some(storage_name),
                read_only: bundle::ReadOnlyMetadata {
                    received_at: file_time,
                    ..Default::default()
                },
                ..Default::default()
            };

            if let Some((bundle, data)) = self.process_received_bundle(data, metadata).await {
                self.ingress_bundle(bundle, data).await;
            }
            metrics::counter!("bpa.restart.orphan").increment(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use hardy_bpv7::{
        bundle::Id,
        eid::{Eid, IpnNodeId, NodeId},
    };

    use super::*;
    use crate::{
        bpa::{Bpa, BpaRegistration},
        storage::{
            BundleMemStorage, BundleStorage, MetadataMemStorage, MetadataStorage,
            Result as StorageResult,
        },
        stream::Sender,
    };

    struct RecordingCla {
        sink: hardy_async::sync::spin::Once<Box<dyn cla::Sink>>,
        offers_tx: flume::Sender<Id>,
    }

    #[async_trait]
    impl cla::Cla for RecordingCla {
        fn lane_count(&self) -> Option<core::num::NonZeroU32> {
            None
        }

        async fn on_register(&self, sink: Box<dyn cla::Sink>, _node_ids: &[NodeId]) {
            self.sink.call_once(|| sink);
        }

        async fn on_unregister(&self) {}

        async fn forward(
            &self,
            _queue: Option<u32>,
            _cla_addr: &cla::ClaAddress,
            bundle_id: &Id,
            _total_len: u64,
            _stream: &mut dyn crate::stream::Receiver<cla::Segment>,
        ) -> cla::Result<cla::ForwardBundleResult> {
            let _ = self.offers_tx.send(bundle_id.clone());
            Ok(cla::ForwardBundleResult::Sent)
        }
    }

    // MetadataMemStorage deliberately opts out of restart recovery
    // (in-memory metadata cannot survive a real restart, so its
    // confirm_exists is a stub). This wrapper answers confirm_exists from
    // the live map, making the replay path reachable in a test that seeds
    // the same storage instance it recovers from. It also raises an event
    // for every status write, so tests synchronize on the transition itself
    // instead of polling for it.
    struct RecoverableMem {
        inner: MetadataMemStorage,
        status_tx: flume::Sender<(Id, bundle::BundleStatus)>,
    }

    impl RecoverableMem {
        fn new() -> (Arc<Self>, flume::Receiver<(Id, bundle::BundleStatus)>) {
            let (status_tx, status_rx) = flume::unbounded();
            (
                Arc::new(Self {
                    inner: MetadataMemStorage::new(None),
                    status_tx,
                }),
                status_rx,
            )
        }

        fn signal(&self, id: &Id, status: &bundle::BundleStatus) {
            let _ = self.status_tx.send((id.clone(), status.clone()));
        }
    }

    // Receives status events until `id` reaches `status`. The timeout only
    // bounds a regression; the assertion itself is event-driven.
    async fn wait_for_status(
        status_rx: &flume::Receiver<(Id, bundle::BundleStatus)>,
        id: &Id,
        status: &bundle::BundleStatus,
    ) {
        loop {
            let (seen_id, seen_status) =
                tokio::time::timeout(tokio::time::Duration::from_secs(5), status_rx.recv_async())
                    .await
                    .unwrap_or_else(|_| panic!("Timeout waiting for {id} to reach {status:?}"))
                    .expect("Status channel closed");
            if seen_id == *id && seen_status == *status {
                return;
            }
        }
    }

    #[async_trait]
    impl MetadataStorage for RecoverableMem {
        async fn get(&self, bundle_id: &Id) -> StorageResult<Option<bundle::Bundle>> {
            self.inner.get(bundle_id).await
        }

        async fn insert(&self, bundle: &bundle::Bundle) -> StorageResult<bool> {
            let inserted = self.inner.insert(bundle).await?;
            if inserted {
                self.signal(&bundle.bundle.id, &bundle.metadata.status);
            }
            Ok(inserted)
        }

        async fn replace(&self, bundle: &bundle::Bundle) -> StorageResult<()> {
            self.inner.replace(bundle).await?;
            self.signal(&bundle.bundle.id, &bundle.metadata.status);
            Ok(())
        }

        async fn update_status(&self, bundle: &bundle::Bundle) -> StorageResult<()> {
            self.inner.update_status(bundle).await?;
            self.signal(&bundle.bundle.id, &bundle.metadata.status);
            Ok(())
        }

        async fn swap_status(
            &self,
            bundle_id: &Id,
            expected: &bundle::BundleStatus,
            status: &bundle::BundleStatus,
        ) -> StorageResult<bool> {
            let swapped = self.inner.swap_status(bundle_id, expected, status).await?;
            if swapped {
                self.signal(bundle_id, status);
            }
            Ok(swapped)
        }

        async fn tombstone_if(
            &self,
            bundle_id: &Id,
            expected: &bundle::BundleStatus,
        ) -> StorageResult<bool> {
            self.inner.tombstone_if(bundle_id, expected).await
        }

        async fn tombstone(&self, bundle_id: &Id) -> StorageResult<()> {
            self.inner.tombstone(bundle_id).await
        }

        async fn start_recovery(&self) {}

        async fn confirm_exists(
            &self,
            bundle_id: &Id,
        ) -> StorageResult<Option<bundle::BundleMetadata>> {
            Ok(self.inner.get(bundle_id).await?.map(|b| b.metadata))
        }

        async fn remove_unconfirmed(
            &self,
            stream: &dyn Sender<bundle::Bundle>,
        ) -> StorageResult<()> {
            self.inner.remove_unconfirmed(stream).await
        }

        async fn reset_peer_queue(&self, peer: u32) -> StorageResult<u64> {
            self.inner.reset_peer_queue(peer).await
        }

        async fn reset_peer_ack_pending(&self, peer: u32) -> StorageResult<u64> {
            self.inner.reset_peer_ack_pending(peer).await
        }

        async fn poll_expiry(
            &self,
            stream: &dyn Sender<bundle::Bundle>,
            limit: usize,
        ) -> StorageResult<()> {
            self.inner.poll_expiry(stream, limit).await
        }

        async fn poll_waiting(&self, stream: &dyn Sender<bundle::Bundle>) -> StorageResult<()> {
            self.inner.poll_waiting(stream).await
        }

        async fn poll_service_waiting(
            &self,
            source: Eid,
            stream: &dyn Sender<bundle::Bundle>,
        ) -> StorageResult<()> {
            self.inner.poll_service_waiting(source, stream).await
        }

        async fn poll_adu_fragments(
            &self,
            stream: &dyn Sender<bundle::Bundle>,
            status: &bundle::BundleStatus,
        ) -> StorageResult<()> {
            self.inner.poll_adu_fragments(stream, status).await
        }

        async fn poll_pending(
            &self,
            stream: &dyn Sender<bundle::Bundle>,
            status: &bundle::BundleStatus,
            limit: usize,
        ) -> StorageResult<()> {
            self.inner.poll_pending(stream, status, limit).await
        }
    }

    // A BundleMemStorage decorator that raises an event for every deletion,
    // so tests synchronize on recovery discarding a copy instead of polling
    // the store.
    struct DeleteSignallingStore {
        inner: BundleMemStorage,
        deleted_tx: flume::Sender<Arc<str>>,
    }

    impl DeleteSignallingStore {
        fn new() -> (Arc<Self>, flume::Receiver<Arc<str>>) {
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

    #[async_trait]
    impl BundleStorage for DeleteSignallingStore {
        async fn recover(
            &self,
            stream: &dyn Sender<crate::storage::RecoveryResponse>,
        ) -> StorageResult<()> {
            self.inner.recover(stream).await
        }

        async fn load(&self, storage_name: &str) -> StorageResult<Option<Bytes>> {
            self.inner.load(storage_name).await
        }

        async fn save(&self, data: Bytes) -> StorageResult<Arc<str>> {
            self.inner.save(data).await
        }

        async fn replace(&self, storage_name: &str, data: Bytes) -> StorageResult<()> {
            self.inner.replace(storage_name, data).await
        }

        async fn delete(&self, storage_name: &str) -> StorageResult<()> {
            self.inner.delete(storage_name).await?;
            let _ = self.deleted_tx.send(Arc::from(storage_name));
            Ok(())
        }
    }

    fn build_bundle_bytes(payload: &[u8]) -> Bytes {
        let (_, data) = hardy_bpv7::builder::Builder::new(
            "ipn:0.3.1".parse().unwrap(),
            "ipn:0.2.99".parse().unwrap(),
        )
        .with_payload(payload.to_vec().into())
        .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
        .unwrap();
        Bytes::from(data)
    }

    fn parse_bundle(data: &Bytes) -> hardy_bpv7::bundle::Bundle {
        hardy_bpv7::bundle::ParsedBundle::parse(data, hardy_bpv7::bpsec::no_keys)
            .unwrap()
            .bundle
    }

    fn test_node_ids() -> crate::node_ids::NodeIds {
        crate::node_ids::NodeIds::try_from(
            [NodeId::Ipn(IpnNodeId {
                allocator_id: 0,
                node_number: 1,
            })]
            .as_slice(),
        )
        .unwrap()
    }

    // Registers a RecordingCla with a peer for ipn:0.2, so recovered
    // bundles destined there get (re-)offered.
    async fn connect_recording_cla(bpa: &Bpa) -> (Arc<RecordingCla>, flume::Receiver<Id>) {
        let (offers_tx, offers_rx) = flume::bounded(16);
        let cla = Arc::new(RecordingCla {
            sink: hardy_async::sync::spin::Once::new(),
            offers_tx,
        });
        bpa.register_cla("recording".to_string(), cla.clone(), None)
            .await
            .unwrap();
        cla.sink
            .get()
            .unwrap()
            .add_peer(
                cla::ClaAddress::Private("peer-2".as_bytes().into()),
                &[NodeId::Ipn(IpnNodeId {
                    allocator_id: 0,
                    node_number: 2,
                })],
            )
            .await
            .unwrap();
        (cla, offers_rx)
    }

    async fn expect_offer(offers_rx: &flume::Receiver<Id>) -> Id {
        // The timeout only bounds a regression; the assertion is event-driven.
        tokio::time::timeout(tokio::time::Duration::from_secs(5), offers_rx.recv_async())
            .await
            .expect("Timeout waiting for offer")
            .expect("Channel closed")
    }

    // A transfer that is still ForwardAckPending when the BPA comes back up
    // (an unclean stop: a clean shutdown's unregistration sweep resolves it
    // first) is outcome-unknown: recovery resets it to Waiting, and it is
    // re-offered once a route to its destination reappears.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restart_resets_ack_pending_transfer() {
        let (metadata_store, status_rx) = RecoverableMem::new();
        let data_store = Arc::new(BundleMemStorage::new(None, None));

        // Seed the stores as an unclean stop would leave them: bundle data
        // present, metadata parked in ForwardAckPending via a now-stale peer
        let data = build_bundle_bytes(b"restart-replay");
        let storage_name = data_store.save(data.clone()).await.unwrap();
        let bundle = bundle::Bundle {
            bundle: parse_bundle(&data),
            metadata: bundle::BundleMetadata {
                status: bundle::BundleStatus::ForwardAckPending { peer: 7 },
                storage_name: Some(storage_name),
                ..Default::default()
            },
        };
        let id = bundle.bundle.id.clone();
        assert!(metadata_store.insert(&bundle).await.unwrap());

        let bpa = Bpa::builder()
            .node_ids(test_node_ids())
            .metadata_storage(metadata_store.clone())
            .bundle_storage(data_store)
            .build()
            .await
            .unwrap();
        bpa.start(true);

        // Recovery replays the stored bundle and resets it to Waiting
        wait_for_status(&status_rx, &id, &bundle::BundleStatus::Waiting).await;

        // A route to the destination appears; the bundle is re-offered
        let (_cla, offers_rx) = connect_recording_cla(&bpa).await;
        assert_eq!(
            id,
            expect_offer(&offers_rx).await,
            "Re-offer must be the recovered bundle"
        );

        bpa.shutdown().await;
    }

    // Unparseable bundle data found at restart is junk: recovery deletes it
    // without inventing metadata for it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restart_deletes_corrupt_data() {
        let (metadata_store, _status_rx) = RecoverableMem::new();
        let (data_store, deleted_rx) = DeleteSignallingStore::new();

        let storage_name = data_store
            .save(Bytes::from_static(b"definitely not a bundle"))
            .await
            .unwrap();

        let bpa = Bpa::builder()
            .node_ids(test_node_ids())
            .metadata_storage(metadata_store)
            .bundle_storage(data_store.clone())
            .build()
            .await
            .unwrap();
        bpa.start(true);

        // The timeout only bounds a regression; the assertion is event-driven.
        let deleted =
            tokio::time::timeout(tokio::time::Duration::from_secs(5), deleted_rx.recv_async())
                .await
                .expect("Timeout waiting for recovery to delete the corrupt data")
                .expect("Channel closed");
        assert_eq!(deleted, storage_name, "The corrupt data must be deleted");
        assert!(data_store.load(&storage_name).await.unwrap().is_none());

        bpa.shutdown().await;
    }

    // A second stored copy of a bundle whose metadata references another
    // copy is a duplicate: recovery deletes the duplicate and leaves the
    // canonical copy and its metadata untouched.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restart_deletes_duplicate_copy() {
        let (metadata_store, _status_rx) = RecoverableMem::new();
        let (data_store, deleted_rx) = DeleteSignallingStore::new();

        let data = build_bundle_bytes(b"restart-duplicate");
        let canonical = data_store.save(data.clone()).await.unwrap();
        let duplicate = data_store.save(data.clone()).await.unwrap();
        assert_ne!(canonical, duplicate);

        let bundle = bundle::Bundle {
            bundle: parse_bundle(&data),
            metadata: bundle::BundleMetadata {
                status: bundle::BundleStatus::Waiting,
                storage_name: Some(canonical.clone()),
                ..Default::default()
            },
        };
        let id = bundle.bundle.id.clone();
        assert!(metadata_store.insert(&bundle).await.unwrap());

        let bpa = Bpa::builder()
            .node_ids(test_node_ids())
            .metadata_storage(metadata_store.clone())
            .bundle_storage(data_store.clone())
            .build()
            .await
            .unwrap();
        bpa.start(true);

        // Exactly the duplicate is discarded: a first deletion of the
        // canonical copy would fail this assertion regardless of the order
        // recovery walks the two copies in.
        // The timeout only bounds a regression; the assertion is event-driven.
        let deleted =
            tokio::time::timeout(tokio::time::Duration::from_secs(5), deleted_rx.recv_async())
                .await
                .expect("Timeout waiting for recovery to delete the duplicate copy")
                .expect("Channel closed");
        assert_eq!(deleted, duplicate, "The duplicate copy must be deleted");

        // The canonical copy and its metadata survive.
        assert!(data_store.load(&canonical).await.unwrap().is_some());
        assert!(data_store.load(&duplicate).await.unwrap().is_none());
        let survivor = metadata_store
            .get(&id)
            .await
            .unwrap()
            .expect("Canonical metadata must survive recovery");
        assert_eq!(survivor.metadata.storage_name, Some(canonical));

        bpa.shutdown().await;
    }

    // Bundle data without metadata is an orphan: recovery replays it
    // through the full receive pipeline, after which it routes like any
    // freshly received bundle.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restart_replays_orphan_data() {
        let (metadata_store, status_rx) = RecoverableMem::new();
        let data_store = Arc::new(BundleMemStorage::new(None, None));

        let data = build_bundle_bytes(b"restart-orphan");
        let id = parse_bundle(&data).id;
        data_store.save(data).await.unwrap();

        let bpa = Bpa::builder()
            .node_ids(test_node_ids())
            .metadata_storage(metadata_store.clone())
            .bundle_storage(data_store)
            .build()
            .await
            .unwrap();
        bpa.start(true);

        // The replayed orphan runs ingress and, with no route to its
        // destination yet, parks as Waiting: metadata has been recreated.
        wait_for_status(&status_rx, &id, &bundle::BundleStatus::Waiting).await;

        // A route to the destination appears; the recovered orphan is offered.
        let (_cla, offers_rx) = connect_recording_cla(&bpa).await;
        assert_eq!(
            id,
            expect_offer(&offers_rx).await,
            "Offer must be the recovered orphan"
        );

        bpa.shutdown().await;
    }

    // An ingress filter that counts its executions, for asserting which
    // recovery paths re-run ingress.
    struct CountingFilter(Arc<core::sync::atomic::AtomicUsize>);

    #[async_trait]
    impl filter::ReadFilter for CountingFilter {
        async fn filter(
            &self,
            _bundle: &bundle::Bundle,
            _data: &[u8],
        ) -> core::result::Result<filter::ReadResult, crate::Error> {
            self.0.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
            Ok(filter::ReadResult::Continue)
        }
    }

    // A bundle checkpointed as Dispatching already passed the Ingress
    // filter before the stop: recovery re-dispatches it for routing without
    // running ingress again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restart_resumes_dispatching_without_ingress() {
        let (metadata_store, status_rx) = RecoverableMem::new();
        let data_store = Arc::new(BundleMemStorage::new(None, None));

        let data = build_bundle_bytes(b"restart-dispatching");
        let storage_name = data_store.save(data.clone()).await.unwrap();
        let bundle = bundle::Bundle {
            bundle: parse_bundle(&data),
            metadata: bundle::BundleMetadata {
                status: bundle::BundleStatus::Dispatching,
                storage_name: Some(storage_name),
                ..Default::default()
            },
        };
        let id = bundle.bundle.id.clone();
        assert!(metadata_store.insert(&bundle).await.unwrap());

        let ingress_runs = Arc::new(core::sync::atomic::AtomicUsize::new(0));
        let bpa = Bpa::builder()
            .node_ids(test_node_ids())
            .metadata_storage(metadata_store.clone())
            .bundle_storage(data_store)
            .filter(
                filter::Hook::Ingress,
                "ingress-probe",
                &[],
                filter::Filter::Read(Arc::new(CountingFilter(ingress_runs.clone()))),
            )
            .build()
            .await
            .unwrap();
        bpa.start(true);

        // Re-dispatch routes the bundle; with no route yet it parks Waiting.
        wait_for_status(&status_rx, &id, &bundle::BundleStatus::Waiting).await;

        let (_cla, offers_rx) = connect_recording_cla(&bpa).await;
        assert_eq!(
            id,
            expect_offer(&offers_rx).await,
            "Offer must be the resumed bundle"
        );

        assert_eq!(
            ingress_runs.load(core::sync::atomic::Ordering::SeqCst),
            0,
            "A Dispatching checkpoint must not re-run the Ingress filter"
        );

        bpa.shutdown().await;
    }
}
