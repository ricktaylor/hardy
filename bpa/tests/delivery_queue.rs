//! Per-service delivery queue lifecycle: bundles queued behind an in-flight
//! delivery hold `DeliverPending`; unregistering the service sweeps them to
//! `WaitingForService` (leaving the in-flight `DeliveryAckPending` claim
//! alone — the service consumes or defers it itself), and a later
//! registration on the same EID delivers them.

use core::{num::NonZeroU32, time::Duration};
use std::{borrow::Cow, sync::Arc};

use hardy_bpa::{
    Bytes, async_trait,
    bpa::{Bpa, BpaRegistration},
    bundle::BundleStatus,
    cla,
    node_ids::NodeIds,
    services,
    storage::{MetadataMemStorage, MetadataStorage},
    stream::{Receiver, Segment},
};
use hardy_bpv7::{
    builder::Builder,
    bundle::Id,
    creation_timestamp::CreationTimestamp,
    eid::{Eid, IpnNodeId, NodeId, Service},
    status_report::ReasonCode,
};

/// Records each delivered bundle id; holds the first delivery open until
/// the release sender fires (or is dropped).
struct HoldService {
    sink: hardy_async::sync::spin::Once<Box<dyn services::ServiceSink>>,
    delivered_tx: flume::Sender<Id>,
    started_tx: flume::Sender<()>,
    release_rx: flume::Receiver<()>,
}

impl HoldService {
    fn new() -> (
        Arc<Self>,
        flume::Receiver<Id>,
        flume::Receiver<()>,
        flume::Sender<()>,
    ) {
        let (delivered_tx, delivered_rx) = flume::unbounded();
        let (started_tx, started_rx) = flume::unbounded();
        let (release_tx, release_rx) = flume::bounded(1);
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                delivered_tx,
                started_tx,
                release_rx,
            }),
            delivered_rx,
            started_rx,
            release_tx,
        )
    }
}

#[async_trait]
impl services::Service for HoldService {
    async fn on_register(&self, _endpoint: &Eid, sink: Box<dyn services::ServiceSink>) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn on_deliver(
        &self,
        bundle_id: &Id,
        _expiry: time::OffsetDateTime,
        _total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        let _ = self.started_tx.send_async(()).await;
        let _ = self.release_rx.recv_async().await;
        loop {
            match stream.recv().await {
                Ok(Segment::Final(_)) | Err(_) => break,
                Ok(_) => {}
            }
        }
        let _ = self.delivered_tx.send_async(bundle_id.clone()).await;
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

struct IngressCla {
    sink: hardy_async::sync::spin::Once<Box<dyn cla::Sink>>,
}

#[async_trait]
impl cla::Cla for IngressCla {
    async fn on_register(&self, sink: Box<dyn cla::Sink>, _node_ids: &[NodeId]) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    fn lane_count(&self) -> Option<NonZeroU32> {
        None
    }

    async fn forward(
        &self,
        _lane: Option<u32>,
        _cla_addr: &cla::ClaAddress,
        _bundle_id: &Id,
        _total_len: u64,
        _stream: &mut dyn Receiver<Segment>,
    ) -> cla::Result<cla::ForwardBundleResult> {
        Ok(cla::ForwardBundleResult::Sent)
    }
}

/// Poll the metadata store until `id` reaches `expected`. Storage writes
/// have no external completion signal, so this is a bounded status wait;
/// the deadline only bounds a regression.
async fn await_status(store: &MetadataMemStorage, id: &Id, expected: &BundleStatus) {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
    loop {
        let status = store
            .get(id)
            .await
            .unwrap()
            .expect("Bundle missing from metadata store")
            .status;
        if status == *expected {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Timeout waiting for {expected:?}, status: {status:?}"
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unregister_sweeps_queued_deliveries() {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let metadata_store = Arc::new(MetadataMemStorage::new(None));
    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .metadata_storage(metadata_store.clone())
        .build()
        .await
        .unwrap();
    bpa.start(false).await;

    let service_eid: Eid = "ipn:0.1.7".parse().unwrap();
    let (svc_a, delivered_a_rx, started_rx, release_tx) = HoldService::new();
    bpa.register_service(Service::Ipn(7), svc_a.clone())
        .await
        .unwrap();

    let cla = Arc::new(IngressCla {
        sink: hardy_async::sync::spin::Once::new(),
    });
    bpa.register_cla("ingress".to_string(), cla.clone(), None)
        .await
        .unwrap();

    let dispatch = |payload: &'static [u8]| {
        let (_, data) = Builder::new("ipn:0.2.1".parse().unwrap(), "ipn:0.1.7".parse().unwrap())
            .with_lifetime(Duration::from_secs(3600))
            .with_payload(Cow::Borrowed(payload))
            .build(CreationTimestamp::now())
            .expect("Failed to build bundle");
        let parsed = hardy_bpv7::parse::parse(Bytes::from(data.clone())).unwrap();
        let id = parsed.bundle.primary.id.clone();
        (id, Bytes::from(data))
    };

    // Bundle 1 is delivered and held open, occupying the service's
    // (serialized) delivery consumer.
    let (_id1, data1) = dispatch(b"held");
    cla.sink
        .get()
        .unwrap()
        .dispatch(None, None, &mut data1.clone())
        .await
        .unwrap();
    tokio::time::timeout(tokio::time::Duration::from_secs(5), started_rx.recv_async())
        .await
        .expect("Timeout waiting for the held delivery")
        .expect("Service gone");

    // Bundle 2 queues behind it in DeliverPending.
    let (id2, data2) = dispatch(b"queued");
    cla.sink
        .get()
        .unwrap()
        .dispatch(None, None, &mut data2.clone())
        .await
        .unwrap();
    await_status(
        &metadata_store,
        &id2,
        &BundleStatus::DeliverPending {
            service: service_eid.clone(),
        },
    )
    .await;

    // Unregistering sweeps the queued bundle to WaitingForService; the held
    // delivery keeps its DeliveryAckPending claim and completes normally.
    svc_a.sink.get().unwrap().unregister().await;
    await_status(
        &metadata_store,
        &id2,
        &BundleStatus::WaitingForService {
            service: service_eid.clone(),
        },
    )
    .await;
    release_tx.send(()).expect("Service gone");
    let delivered = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        delivered_a_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for the held delivery to complete")
    .expect("Service gone");
    assert_ne!(delivered, id2, "the swept bundle must not reach service A");

    // A later registration on the same EID recovers the swept bundle.
    let (svc_b, delivered_b_rx, _started_b_rx, release_b_tx) = HoldService::new();
    drop(release_b_tx); // complete deliveries immediately
    bpa.register_service(Service::Ipn(7), svc_b.clone())
        .await
        .unwrap();
    let delivered = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        delivered_b_rx.recv_async(),
    )
    .await
    .expect("Timeout waiting for the swept bundle's redelivery")
    .expect("Service gone");
    assert_eq!(
        delivered, id2,
        "the swept bundle is redelivered on registration"
    );

    bpa.shutdown().await;
    assert!(
        delivered_a_rx.is_empty() && delivered_b_rx.is_empty(),
        "no duplicate deliveries"
    );
}
