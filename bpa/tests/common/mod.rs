//! Shared test doubles and helpers for the BPA integration tests.
//!
//! Each integration test binary compiles this module independently, so items
//! used by other test files appear unused in each binary.
#![allow(dead_code)]

use std::sync::Arc;

use hardy_bpa::{
    Bytes, async_trait,
    bundle::{Bundle, BundleMetadata, BundleStatus},
    node_ids::NodeIds,
    storage::{MetadataMemStorage, MetadataStorage, backend},
    stream::{Segment, Sender},
};
use hardy_bpv7::{
    bundle::Id,
    eid::{Eid, IpnNodeId, NodeId},
};

/// The node id `ipn:0.<node_number>`.
pub fn ipn_node(node_number: u32) -> NodeId {
    NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number,
    })
}

/// The single-entry `NodeIds` for `ipn:0.<node_number>`.
pub fn node_ids(node_number: u32) -> NodeIds {
    NodeIds::try_from([ipn_node(node_number)].as_slice()).unwrap()
}

/// Builds a bundle, returning its encoded bytes.
pub fn build_bundle(source: &Eid, destination: &Eid, payload: &[u8]) -> Bytes {
    let (_, data) = hardy_bpv7::builder::Builder::new(source.clone(), destination.clone())
        .with_payload(std::borrow::Cow::Borrowed(payload))
        .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
        .expect("Failed to build bundle");
    Bytes::from(data)
}

/// The identity of a bundle built by [`build_bundle`], for direct
/// `forward`/`on_deliver` calls.
pub fn bundle_id_of(data: &Bytes) -> Id {
    hardy_bpv7::bundle::ParsedBundle::parse(data, hardy_bpv7::bpsec::no_keys)
        .expect("Failed to parse built bundle")
        .bundle
        .id
}

/// A pre-filled segment stream. The sender is dropped on return, so a
/// sequence not ending in `Final` reads as a truncated stream.
pub async fn feed(segments: Vec<Segment>) -> hardy_async::channel::Receiver<Segment> {
    let (tx, rx) = hardy_async::channel::bounded(segments.len().max(1));
    for segment in segments {
        tx.send(segment).await.unwrap();
    }
    rx
}

/// Receives the next mock event from `rx`, panicking after `secs` seconds.
pub async fn recv_event<T>(rx: &flume::Receiver<T>, secs: u64) -> T {
    tokio::time::timeout(tokio::time::Duration::from_secs(secs), rx.recv_async())
        .await
        .expect("Timed out waiting for event")
        .expect("Event channel closed")
}

/// A `MetadataMemStorage` decorator that raises an event whenever a bundle
/// is parked as `WaitingForService`, so a test can synchronize on the park
/// itself instead of guessing at its timing with sleeps: by the time the
/// signal arrives the status is in the store, and the next registration's
/// `poll_service_waiting` must find the bundle.
pub struct ParkSignallingStore {
    inner: MetadataMemStorage,
    parked_tx: flume::Sender<Id>,
}

impl ParkSignallingStore {
    pub fn new() -> (Arc<Self>, flume::Receiver<Id>) {
        let (parked_tx, parked_rx) = flume::unbounded();
        (
            Arc::new(Self {
                inner: MetadataMemStorage::new(None),
                parked_tx,
            }),
            parked_rx,
        )
    }

    fn signal_if_parked(&self, id: &Id, status: &BundleStatus) {
        if matches!(status, BundleStatus::WaitingForService { .. }) {
            let _ = self.parked_tx.send(id.clone());
        }
    }
}

#[async_trait]
impl MetadataStorage for ParkSignallingStore {
    async fn get(&self, bundle_id: &Id) -> backend::Result<Option<Bundle>> {
        self.inner.get(bundle_id).await
    }

    async fn insert(&self, bundle: &Bundle) -> backend::Result<bool> {
        self.inner.insert(bundle).await
    }

    async fn replace(&self, bundle: &Bundle) -> backend::Result<()> {
        self.inner.replace(bundle).await
    }

    async fn update_status(&self, bundle: &Bundle) -> backend::Result<()> {
        self.inner.update_status(bundle).await?;
        self.signal_if_parked(&bundle.bundle.id, &bundle.metadata.status);
        Ok(())
    }

    async fn swap_status(
        &self,
        bundle_id: &Id,
        expected: &BundleStatus,
        status: &BundleStatus,
    ) -> backend::Result<bool> {
        let swapped = self.inner.swap_status(bundle_id, expected, status).await?;
        if swapped {
            self.signal_if_parked(bundle_id, status);
        }
        Ok(swapped)
    }

    async fn tombstone_if(&self, bundle_id: &Id, expected: &BundleStatus) -> backend::Result<bool> {
        self.inner.tombstone_if(bundle_id, expected).await
    }

    async fn tombstone(&self, bundle_id: &Id) -> backend::Result<()> {
        self.inner.tombstone(bundle_id).await
    }

    async fn start_recovery(&self) {
        self.inner.start_recovery().await
    }

    async fn confirm_exists(&self, bundle_id: &Id) -> backend::Result<Option<BundleMetadata>> {
        self.inner.confirm_exists(bundle_id).await
    }

    async fn remove_unconfirmed(&self, stream: &dyn Sender<Bundle>) -> backend::Result<()> {
        self.inner.remove_unconfirmed(stream).await
    }

    async fn reset_peer_queue(&self, peer: u32) -> backend::Result<u64> {
        self.inner.reset_peer_queue(peer).await
    }

    async fn reset_peer_ack_pending(&self, peer: u32) -> backend::Result<u64> {
        self.inner.reset_peer_ack_pending(peer).await
    }

    async fn poll_expiry(&self, stream: &dyn Sender<Bundle>, limit: usize) -> backend::Result<()> {
        self.inner.poll_expiry(stream, limit).await
    }

    async fn poll_waiting(&self, stream: &dyn Sender<Bundle>) -> backend::Result<()> {
        self.inner.poll_waiting(stream).await
    }

    async fn poll_service_waiting(
        &self,
        source: Eid,
        stream: &dyn Sender<Bundle>,
    ) -> backend::Result<()> {
        self.inner.poll_service_waiting(source, stream).await
    }

    async fn poll_adu_fragments(
        &self,
        stream: &dyn Sender<Bundle>,
        status: &BundleStatus,
    ) -> backend::Result<()> {
        self.inner.poll_adu_fragments(stream, status).await
    }

    async fn poll_pending(
        &self,
        stream: &dyn Sender<Bundle>,
        status: &BundleStatus,
        limit: usize,
    ) -> backend::Result<()> {
        self.inner.poll_pending(stream, status, limit).await
    }
}
