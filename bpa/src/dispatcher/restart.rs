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

        // Validate the stored bundle data is not corrupt. We use the
        // Preserve-mode parse (rather than the full ingress pipeline) because the
        // bundle was already fully processed at ingress — restart should verify
        // integrity and resume, not re-apply block removal or canonicalization.
        // Soft NoKey: re-checking already-accepted data, so a key that has since
        // rotated away mustn't drop the bundle — the `nokey_ext` facts are ignored.
        let bundle = match crate::bundle::parse::parse_validate_with_provider(
            data.clone(),
            self.key_provider(),
        ) {
            Ok((bundle, _nokey)) => bundle,
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
            // pipeline (process_received_bundle: parse, block removal,
            // canonicalization, storage, reporting, and Ingress filter).
            let metadata = bundle::BundleMetadata {
                status: bundle::BundleStatus::New,
                storage_name: Some(storage_name.clone()),
                read_only: bundle::ReadOnlyMetadata {
                    received_at: file_time,
                    ..Default::default()
                },
                ..Default::default()
            };

            // TODO: Just push the entire bundle into the stream
            let (tx, mut rx) = hardy_async::channel::bounded(1);
            tx.send(crate::stream::Segment::Final(data))
                .await
                .trace_expect("New stream push failed?!?");

            match self.process_received_bundle(&mut rx, metadata).await {
                Ok(Some((bundle, data))) => self.ingress_bundle(bundle, data).await,
                // Re-validation rejected the orphan — delete its stranded data.
                Ok(None) => {
                    self.store.delete_data(&storage_name).await;
                }
                // A stored orphan that trips the gate has no live transfer to
                // refuse — log, and delete its stranded data.
                Err(e) => {
                    warn!("Restart orphan rejected: {e}");
                    self.store.delete_data(&storage_name).await;
                }
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
        services::{Application, ApplicationSink, StatusNotify},
        storage::{
            BundleMemStorage, BundleStorage, MetadataMemStorage, MetadataStorage,
            Result as StorageResult,
        },
        stream::{Receiver, Segment, Sender},
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
    // the same storage instance it recovers from. It also nudges a channel
    // after every status write, so a test waits on the transition itself
    // rather than polling on a timer.
    struct RecoverableMem {
        store: MetadataMemStorage,
        nudge_tx: flume::Sender<()>,
    }

    impl RecoverableMem {
        fn new() -> (Arc<Self>, flume::Receiver<()>) {
            let (nudge_tx, nudge_rx) = flume::unbounded();
            (
                Arc::new(Self {
                    store: MetadataMemStorage::new(None),
                    nudge_tx,
                }),
                nudge_rx,
            )
        }
    }

    #[async_trait]
    impl MetadataStorage for RecoverableMem {
        async fn get(&self, bundle_id: &Id) -> StorageResult<Option<bundle::Bundle>> {
            self.store.get(bundle_id).await
        }

        async fn insert(&self, bundle: &bundle::Bundle) -> StorageResult<bool> {
            self.store.insert(bundle).await
        }

        async fn replace(&self, bundle: &bundle::Bundle) -> StorageResult<()> {
            self.store.replace(bundle).await
        }

        async fn update_status(&self, bundle: &bundle::Bundle) -> StorageResult<()> {
            let result = self.store.update_status(bundle).await;
            let _ = self.nudge_tx.send(());
            result
        }

        async fn swap_status(
            &self,
            bundle_id: &Id,
            expected: &bundle::BundleStatus,
            status: &bundle::BundleStatus,
        ) -> StorageResult<bool> {
            let swapped = self.store.swap_status(bundle_id, expected, status).await;
            let _ = self.nudge_tx.send(());
            swapped
        }

        async fn tombstone_if(
            &self,
            bundle_id: &Id,
            expected: &bundle::BundleStatus,
        ) -> StorageResult<bool> {
            let tombstoned = self.store.tombstone_if(bundle_id, expected).await;
            let _ = self.nudge_tx.send(());
            tombstoned
        }

        async fn tombstone(&self, bundle_id: &Id) -> StorageResult<()> {
            let result = self.store.tombstone(bundle_id).await;
            let _ = self.nudge_tx.send(());
            result
        }

        async fn start_recovery(&self) {}

        async fn confirm_exists(
            &self,
            bundle_id: &Id,
        ) -> StorageResult<Option<bundle::BundleMetadata>> {
            Ok(self.store.get(bundle_id).await?.map(|b| b.metadata))
        }

        async fn remove_unconfirmed(
            &self,
            stream: &dyn Sender<bundle::Bundle>,
        ) -> StorageResult<()> {
            self.store.remove_unconfirmed(stream).await
        }

        async fn reset_peer_queue(&self, peer: u32) -> StorageResult<u64> {
            self.store.reset_peer_queue(peer).await
        }

        async fn reset_peer_ack_pending(&self, peer: u32) -> StorageResult<u64> {
            self.store.reset_peer_ack_pending(peer).await
        }

        async fn poll_expiry(
            &self,
            stream: &dyn Sender<bundle::Bundle>,
            limit: usize,
        ) -> StorageResult<()> {
            self.store.poll_expiry(stream, limit).await
        }

        async fn poll_waiting(&self, stream: &dyn Sender<bundle::Bundle>) -> StorageResult<()> {
            self.store.poll_waiting(stream).await
        }

        async fn poll_service_waiting(
            &self,
            source: Eid,
            stream: &dyn Sender<bundle::Bundle>,
        ) -> StorageResult<()> {
            self.store.poll_service_waiting(source, stream).await
        }

        async fn poll_adu_fragments(
            &self,
            stream: &dyn Sender<bundle::Bundle>,
            status: &bundle::BundleStatus,
        ) -> StorageResult<()> {
            self.store.poll_adu_fragments(stream, status).await
        }

        async fn poll_pending(
            &self,
            stream: &dyn Sender<bundle::Bundle>,
            status: &bundle::BundleStatus,
            limit: usize,
        ) -> StorageResult<()> {
            self.store.poll_pending(stream, status, limit).await
        }
    }

    // A transfer that is still ForwardAckPending when the BPA comes back up
    // (an unclean stop: a clean shutdown's unregistration sweep resolves it
    // first) is outcome-unknown: recovery resets it to Waiting, and it is
    // re-offered once a route to its destination reappears.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restart_resets_ack_pending_transfer() {
        let (metadata_store, nudge_rx) = RecoverableMem::new();
        let data_store = Arc::new(BundleMemStorage::new(None, None));

        // Seed the stores as an unclean stop would leave them: bundle data
        // present, metadata parked in ForwardAckPending via a now-stale peer
        let (_, data) = hardy_bpv7::builder::Builder::new(
            "ipn:0.3.1".parse().unwrap(),
            "ipn:0.2.99".parse().unwrap(),
        )
        .with_payload(b"restart-replay".to_vec().into())
        .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
        .unwrap();
        let data = Bytes::from(data);
        let storage_name = data_store.save(data.clone()).await.unwrap();
        let (parsed, _) = crate::bundle::parse::parse_validate_with_provider(
            data.clone(),
            hardy_bpv7::bpsec::no_keys,
        )
        .unwrap();
        let bundle = bundle::Bundle {
            bundle: parsed,
            metadata: bundle::BundleMetadata {
                status: bundle::BundleStatus::ForwardAckPending { peer: 7 },
                storage_name: Some(storage_name),
                ..Default::default()
            },
        };
        let id = bundle.bundle.id.clone();
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
            .build()
            .await
            .unwrap();
        bpa.start(true);

        // Recovery replays the stored bundle and resets it to Waiting; the
        // store nudges on every status write, so re-read the live status on
        // each nudge rather than polling on a timer.
        loop {
            let status = metadata_store
                .get(&id)
                .await
                .unwrap()
                .expect("Recovered bundle missing from metadata store")
                .metadata
                .status;
            if status == bundle::BundleStatus::Waiting {
                break;
            }
            // The timeout only bounds a regression.
            tokio::time::timeout(tokio::time::Duration::from_secs(5), nudge_rx.recv_async())
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "Timed out waiting for recovery to reset the transfer, status: {status:?}"
                    )
                })
                .expect("nudge channel closed");
        }

        // A route to the destination appears; the bundle is re-offered
        let (offers_tx, offers_rx) = flume::bounded(16);
        let cla = Arc::new(RecordingCla {
            sink: hardy_async::sync::spin::Once::new(),
            offers_tx,
        });
        bpa.register_cla("recording-2".to_string(), cla.clone(), None)
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

        let id2 = tokio::time::timeout(tokio::time::Duration::from_secs(5), offers_rx.recv_async())
            .await
            .expect("Timeout waiting for re-offer")
            .expect("Channel closed");
        assert_eq!(id, id2, "Re-offer must be the recovered bundle");

        bpa.shutdown().await;
    }

    // An application stash for the recovery test: buffers the announced
    // payload and hands it to the test.
    struct StashApp {
        sink: hardy_async::sync::spin::Once<Box<dyn ApplicationSink>>,
        payloads_tx: flume::Sender<(Id, Bytes)>,
    }

    #[async_trait]
    impl Application for StashApp {
        async fn on_register(&self, _source: &Eid, sink: Box<dyn ApplicationSink>) {
            self.sink.call_once(|| sink);
        }

        async fn on_unregister(&self) {}

        async fn on_deliver(
            &self,
            bundle_id: &Id,
            _expiry: time::OffsetDateTime,
            _ack_requested: bool,
            adu_size: u64,
            stream: &mut dyn Receiver<Segment>,
        ) -> services::Result<()> {
            let payload = crate::stream::buffer_stream(stream, adu_size).await?;
            let _ = self.payloads_tx.send((bundle_id.clone(), payload));
            Ok(())
        }

        async fn on_status_notify(
            &self,
            _bundle_id: &Id,
            _from: &Eid,
            _kind: StatusNotify,
            _reason: hardy_bpv7::status_report::ReasonCode,
            _timestamp: Option<time::OffsetDateTime>,
        ) {
        }
    }

    // A bundle parked WaitingForService survives a restart: recovery
    // replays it, the service's next registration is announced the
    // bundle afresh, and the fresh stream collects it whole.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restart_re_announces_waiting_for_service() {
        let (metadata_store, _nudge_rx) = RecoverableMem::new();
        let data_store = Arc::new(BundleMemStorage::new(None, None));

        // Seed the stores as a stop would leave them: data present,
        // metadata parked for the service endpoint ipn:1.42.
        let (raw, data) = hardy_bpv7::builder::Builder::new(
            "ipn:0.3.1".parse().unwrap(),
            "ipn:1.42".parse().unwrap(),
        )
        .with_payload(b"survives restart".to_vec().into())
        .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
        .unwrap();
        let data = Bytes::from(data);
        let storage_name = data_store.save(data.clone()).await.unwrap();
        let parsed = crate::bundle::parse::rich_from_built(raw, &data).unwrap();
        let bundle = bundle::Bundle {
            bundle: parsed,
            metadata: bundle::BundleMetadata {
                status: bundle::BundleStatus::WaitingForService {
                    service: "ipn:1.42".parse().unwrap(),
                },
                storage_name: Some(storage_name),
                ..Default::default()
            },
        };
        let id = bundle.bundle.id.clone();
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
            .build()
            .await
            .unwrap();
        bpa.start(true);

        let (payloads_tx, payloads_rx) = flume::bounded(4);
        let app = Arc::new(StashApp {
            sink: hardy_async::sync::spin::Once::new(),
            payloads_tx,
        });
        bpa.register_application(hardy_bpv7::eid::Service::Ipn(42), app.clone())
            .await
            .unwrap();

        let (announced, payload) = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            payloads_rx.recv_async(),
        )
        .await
        .expect("the recovered bundle must be re-delivered")
        .unwrap();
        assert_eq!(announced, id);
        assert_eq!(payload.as_ref(), b"survives restart");

        // Completion resolves the recovered bundle terminally: shutdown
        // joins the worker pool, so the resolution has happened by the
        // time it returns.
        bpa.shutdown().await;
        assert!(
            metadata_store.get(&id).await.unwrap().is_none(),
            "The collected delivery must resolve terminally"
        );
    }
}
