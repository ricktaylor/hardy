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
            Ok((bundle, _extracted, _nokey)) => bundle,
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
        if let Some((metadata, status)) = self.store.confirm_exists(&bundle.primary.id).await {
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

            // Resume processing based on the checkpoint status, pairing the
            // stored record with the freshly-parsed bundle — the bytes on
            // disk stay authoritative for the wire half.
            let bundle = bundle::Bundle {
                bpv7: bundle,
                metadata,
                status,
            };
            match &bundle.status {
                bundle::BundleStatus::New => {
                    // Ingress filter not yet complete — run full ingress
                    self.ingress_bundle(bundle, data).await;
                }
                bundle::BundleStatus::Dispatching => {
                    // Ingress filter done — enqueue for routing
                    metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&bundle.status)).increment(1.0);
                    self.dispatch_bundle(bundle).await;
                }
                bundle::BundleStatus::ForwardPending { .. }
                | bundle::BundleStatus::ForwardAckPending { .. } => {
                    // Peer IDs and CLA registrations are stale after restart —
                    // queued bundles re-route, and an in-flight transfer's
                    // outcome can never arrive (outcome-unknown) — reset to
                    // Waiting
                    let mut bundle = bundle;
                    metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&bundle.status)).increment(1.0);
                    self.store
                        .update_status(&mut bundle, &bundle::BundleStatus::Waiting)
                        .await;
                }
                // Other statuses are handled by their respective recovery mechanisms:
                // - Waiting: poll_waiting recovery
                // - WaitingForService: poll_service_waiting on service re-registration
                // - AduFragment: fragment reassembly polling
                _ => {
                    metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&bundle.status)).increment(1.0);
                }
            }
        } else {
            // Orphan — data exists but no metadata. Run the full receive
            // pipeline (process_received_bundle: parse, block removal,
            // canonicalization, storage, reporting, and Ingress filter).
            let mut metadata = bundle::BundleMetadata::new(file_time, bundle::Origin::Recovered);
            metadata.storage_name = Some(storage_name.clone());

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
    // the same storage instance it recovers from.
    struct RecoverableMem(MetadataMemStorage);

    #[async_trait]
    impl MetadataStorage for RecoverableMem {
        async fn get(&self, bundle_id: &Id) -> StorageResult<Option<bundle::Bundle>> {
            self.0.get(bundle_id).await
        }

        async fn insert(&self, bundle: &bundle::Bundle) -> StorageResult<bool> {
            self.0.insert(bundle).await
        }

        async fn replace(&self, bundle: &bundle::Bundle) -> StorageResult<()> {
            self.0.replace(bundle).await
        }

        async fn update_status(
            &self,
            bundle_id: &Id,
            status: &bundle::BundleStatus,
        ) -> StorageResult<()> {
            self.0.update_status(bundle_id, status).await
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

        async fn confirm_exists(
            &self,
            bundle_id: &Id,
        ) -> StorageResult<Option<(bundle::BundleMetadata, bundle::BundleStatus)>> {
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

        async fn poll_expiry(
            &self,
            stream: &dyn Sender<bundle::Bundle>,
            limit: usize,
        ) -> StorageResult<()> {
            self.0.poll_expiry(stream, limit).await
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

    // A transfer that is still ForwardAckPending when the BPA comes back up
    // (an unclean stop: a clean shutdown's unregistration sweep resolves it
    // first) is outcome-unknown: recovery resets it to Waiting, and it is
    // re-offered once a route to its destination reappears.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restart_resets_ack_pending_transfer() {
        let metadata_store = Arc::new(RecoverableMem(MetadataMemStorage::new(None)));
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
        let (parsed, _, _) = crate::bundle::parse::parse_validate_with_provider(
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
            .build()
            .await
            .unwrap();
        bpa.start(true);

        // Recovery replays the stored bundle and resets it to Waiting
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
        loop {
            let status = metadata_store
                .get(&id)
                .await
                .unwrap()
                .expect("Recovered bundle missing from metadata store")
                .status;
            if status == bundle::BundleStatus::Waiting {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "Timeout waiting for recovery to reset the transfer, status: {status:?}"
            );
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
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
}
