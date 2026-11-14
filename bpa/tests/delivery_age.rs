//! The bundle age `on_deliver` passes: the time since creation under the
//! RFC 9171 §5.5 rule, measured at delivery. A clocked bundle's age runs
//! from its creation timestamp; an unclocked bundle's is its Bundle Age at
//! arrival plus its time in this BPA. One test drives each delivery door.

use core::{num::NonZeroU64, time::Duration};
use std::{borrow::Cow, sync::Arc};

use hardy_bpa::{
    Bytes, async_trait,
    bpa::{Bpa, BpaRegistration},
    cla,
    node_ids::NodeIds,
    services,
    storage::{BundleMemStorage, BundleStorage, RecoveryResponse, Result as StorageResult},
    stream::{Receiver, Segment, SendError, Sender},
};
use hardy_bpv7::{
    block::Type,
    builder::Builder,
    bundle::Id,
    creation_timestamp::CreationTimestamp,
    eid::{Eid, IpnNodeId, NodeId, Service},
    status_report::ReasonCode,
};
use hardy_cbor::encode::emit;
use time::OffsetDateTime;

const SOURCE: &str = "ipn:0.2.1";
const DESTINATION: &str = "ipn:0.1.7";

// What a delivery door saw: the bundle, the age it was passed, and when the
// door was entered, which is no earlier than when the BPA measured the age.
struct Delivery {
    bundle_id: Id,
    age: Duration,
    entered_at: OffsetDateTime,
}

// ---------------------------------------------------------------------------
// Recording services, one per delivery door
// ---------------------------------------------------------------------------

struct AgeApp {
    sink: hardy_async::sync::spin::Once<Box<dyn services::ApplicationSink>>,
    delivered_tx: flume::Sender<Delivery>,
}

impl AgeApp {
    fn new() -> (Arc<Self>, flume::Receiver<Delivery>) {
        let (delivered_tx, delivered_rx) = flume::bounded(16);
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                delivered_tx,
            }),
            delivered_rx,
        )
    }
}

#[async_trait]
impl services::Application for AgeApp {
    async fn on_register(&self, _source: &Eid, sink: Box<dyn services::ApplicationSink>) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn on_deliver(
        &self,
        bundle_id: &Id,
        _expiry: OffsetDateTime,
        age: Duration,
        _ack_requested: bool,
        _total_len: u64,
        _stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        let entered_at = OffsetDateTime::now_utc();
        let _ = self.delivered_tx.send(Delivery {
            bundle_id: bundle_id.clone(),
            age,
            entered_at,
        });
        Ok(())
    }

    async fn on_status_notify(
        &self,
        _bundle_id: &Id,
        _from: &Eid,
        _kind: services::StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<OffsetDateTime>,
    ) {
    }
}

struct AgeService {
    sink: hardy_async::sync::spin::Once<Box<dyn services::ServiceSink>>,
    delivered_tx: flume::Sender<Delivery>,
}

impl AgeService {
    fn new() -> (Arc<Self>, flume::Receiver<Delivery>) {
        let (delivered_tx, delivered_rx) = flume::bounded(16);
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                delivered_tx,
            }),
            delivered_rx,
        )
    }
}

#[async_trait]
impl services::Service for AgeService {
    async fn on_register(&self, _endpoint: &Eid, sink: Box<dyn services::ServiceSink>) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn on_deliver(
        &self,
        bundle_id: &Id,
        _expiry: OffsetDateTime,
        age: Duration,
        _total_len: u64,
        _stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        let entered_at = OffsetDateTime::now_utc();
        let _ = self.delivered_tx.send(Delivery {
            bundle_id: bundle_id.clone(),
            age,
            entered_at,
        });
        Ok(())
    }

    async fn on_status_notify(
        &self,
        _bundle_id: &Id,
        _from: &Eid,
        _kind: services::StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<OffsetDateTime>,
    ) {
    }
}

// ---------------------------------------------------------------------------
// Bundle sources: a CLA, and storage that has held a bundle for a while
// ---------------------------------------------------------------------------

struct IngressCla {
    sink: hardy_async::sync::spin::Once<Box<dyn cla::Sink>>,
}

#[async_trait]
impl cla::Cla for IngressCla {
    async fn on_register(
        &self,
        sink: Box<dyn cla::Sink>,
        _node_ids: &[NodeId],
        _max_bundle_size: Option<NonZeroU64>,
    ) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

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

// Bundle storage whose recovery reports every bundle as stored `residency`
// earlier than it was: what a restart finds after the BPA has held a bundle
// that long. Recovery admits it as received at that time.
struct HeldStorage {
    inner: BundleMemStorage,
    residency: Duration,
}

#[async_trait]
impl BundleStorage for HeldStorage {
    async fn recover(&self, stream: &dyn Sender<RecoveryResponse>) -> StorageResult<()> {
        self.inner
            .recover(&Backdated {
                stream,
                by: self.residency,
            })
            .await
    }

    async fn load(&self, storage_name: &str) -> StorageResult<Option<Bytes>> {
        self.inner.load(storage_name).await
    }

    async fn save(&self, data: Bytes) -> StorageResult<Arc<str>> {
        self.inner.save(data).await
    }

    async fn delete(&self, storage_name: &str) -> StorageResult<()> {
        self.inner.delete(storage_name).await
    }
}

// Moves each recovered entry's storage time `by` earlier.
struct Backdated<'a> {
    stream: &'a dyn Sender<RecoveryResponse>,
    by: Duration,
}

#[async_trait]
impl Sender<RecoveryResponse> for Backdated<'_> {
    async fn send(&self, item: RecoveryResponse) -> Result<(), SendError<RecoveryResponse>> {
        let (storage_name, stored_at) = item;
        self.stream.send((storage_name, stored_at - self.by)).await
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn node_ids() -> NodeIds {
    NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap()
}

// `later - earlier`; the tests only take it forward in time.
fn elapsed(earlier: OffsetDateTime, later: OffsetDateTime) -> Duration {
    (later - earlier)
        .try_into()
        .expect("the wall clock went backwards")
}

async fn recv_delivery(rx: &flume::Receiver<Delivery>) -> Delivery {
    // Event-driven wait; the timeout only bounds a regression.
    tokio::time::timeout(tokio::time::Duration::from_secs(5), rx.recv_async())
        .await
        .expect("Timed out waiting for the delivery")
        .expect("Delivery channel closed")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// A clocked bundle created ten minutes before it is sent is delivered with
// the time since its creation timestamp, through the Application door.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clocked_bundle_age_runs_from_creation_timestamp() {
    let bpa = Bpa::builder().node_ids(node_ids()).build().await.unwrap();
    bpa.start(false).await;

    let (app, delivered_rx) = AgeApp::new();
    bpa.register_application(Service::Ipn(7), app.clone())
        .await
        .unwrap();
    let cla = Arc::new(IngressCla {
        sink: hardy_async::sync::spin::Once::new(),
    });
    bpa.register_cla(
        "ingress".to_string(),
        cla.clone(),
        None,
        cla::ClaInit::default(),
    )
    .await
    .unwrap();

    let created = OffsetDateTime::now_utc() - Duration::from_secs(600);
    let (_, data) = Builder::new(SOURCE.parse().unwrap(), DESTINATION.parse().unwrap())
        .with_payload(Cow::Borrowed(b"clocked".as_slice()))
        .build(CreationTimestamp::try_from(created).unwrap())
        .expect("Failed to build bundle");
    let sent_at = OffsetDateTime::now_utc();
    assert_eq!(
        cla.sink
            .get()
            .unwrap()
            .dispatch(None, None, &mut Bytes::from(data))
            .await
            .unwrap(),
        cla::Acceptance::Accepted
    );

    let delivery = recv_delivery(&delivered_rx).await;
    // The creation timestamp as encoded, in whole milliseconds.
    let created = delivery
        .bundle_id
        .timestamp
        .as_datetime()
        .expect("the bundle is clocked");
    let (least, most) = (
        elapsed(created, sent_at),
        elapsed(created, delivery.entered_at),
    );
    assert!(
        (least..=most).contains(&delivery.age),
        "age {:?} is not the time since creation, {least:?}..={most:?}",
        delivery.age
    );

    bpa.shutdown().await;
}

// An unclocked bundle that arrived a minute old (its Bundle Age block) and
// was then held in storage for five minutes is delivered with both, through
// the Service door.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unclocked_bundle_age_adds_time_in_this_bpa() {
    const BUNDLE_AGE_MS: u64 = 60_000;
    let residency = Duration::from_secs(300);

    let (_, data) = Builder::new(SOURCE.parse().unwrap(), DESTINATION.parse().unwrap())
        .add_extension_block(Type::BundleAge)
        .unwrap()
        .build(emit(&BUNDLE_AGE_MS).0.into())
        .with_payload(Cow::Borrowed(b"unclocked".as_slice()))
        .build(CreationTimestamp::default())
        .expect("Failed to build bundle");
    let storage = Arc::new(HeldStorage {
        inner: BundleMemStorage::new(None, None),
        residency,
    });
    let saved_at = OffsetDateTime::now_utc();
    storage.save(Bytes::from(data)).await.unwrap();

    let bpa = Bpa::builder()
        .node_ids(node_ids())
        .bundle_storage(storage)
        .build()
        .await
        .unwrap();
    // Recovery admits the stored bundle, received `residency` ago; with no
    // service registered yet, it waits for one.
    bpa.start(true).await;

    let (svc, delivered_rx) = AgeService::new();
    bpa.register_service(Service::Ipn(7), svc.clone())
        .await
        .unwrap();

    let delivery = recv_delivery(&delivered_rx).await;
    assert!(
        !delivery.bundle_id.timestamp.is_clocked(),
        "the delivered bundle is the unclocked one"
    );
    let least = Duration::from_millis(BUNDLE_AGE_MS) + residency;
    let most = least + elapsed(saved_at, delivery.entered_at);
    assert!(
        (least..=most).contains(&delivery.age),
        "age {:?} is not the Bundle Age plus the time held, {least:?}..={most:?}",
        delivery.age
    );

    bpa.shutdown().await;
}
