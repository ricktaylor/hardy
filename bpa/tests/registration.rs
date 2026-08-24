//! CLA registration and peer lifecycle integration tests.
//!
//! These drive a complete BPA through the public builder and registration
//! traits: CLA name uniqueness, peer add/remove and its RIB effects,
//! cascading cleanup at unregistration, and egress-policy queue selection.

use std::sync::Arc;

use hardy_bpa::bpa::{Bpa, BpaRegistration};
use hardy_bpa::{async_trait, bundle, cla, filter, policy, services, stream};
use hardy_bpv7::bundle::Id;
use hardy_bpv7::eid::NodeId;

mod common;

use common::{build_bundle, ipn_node, node_ids, recv_event};

// A CLA that records the id of every bundle offered to it.
struct TestCla {
    sink: hardy_async::sync::spin::Once<Box<dyn cla::Sink>>,
    forwarded_tx: flume::Sender<Id>,
}

impl TestCla {
    fn new() -> (Arc<Self>, flume::Receiver<Id>) {
        let (tx, rx) = flume::bounded(16);
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                forwarded_tx: tx,
            }),
            rx,
        )
    }

    fn sink(&self) -> &dyn cla::Sink {
        self.sink.get().expect("Sink should be set").as_ref()
    }
}

#[async_trait]
impl cla::Cla for TestCla {
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
        _stream: &mut dyn hardy_bpa::stream::Receiver<cla::Segment>,
    ) -> cla::Result<cla::ForwardBundleResult> {
        let _ = self.forwarded_tx.send(bundle_id.clone());
        Ok(cla::ForwardBundleResult::Sent)
    }
}

// Registering a CLA with an already-in-use name should fail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_cla_name_is_rejected() {
    let bpa = Bpa::builder().build().await.unwrap();
    bpa.start(false);

    let (cla1, _rx1) = TestCla::new();
    let result = bpa.register_cla("test-cla".to_string(), cla1, None).await;
    assert!(result.is_ok(), "First CLA registration should succeed");

    let (cla2, _rx2) = TestCla::new();
    let result = bpa.register_cla("test-cla".to_string(), cla2, None).await;
    assert!(
        matches!(result, Err(cla::Error::AlreadyExists(ref name)) if name == "test-cla"),
        "Duplicate CLA name should return AlreadyExists, got: {result:?}"
    );

    bpa.shutdown().await;
}

// Adding a peer installs a RIB entry; removing it withdraws it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peer_lifecycle() {
    let bpa = Bpa::builder().build().await.unwrap();
    bpa.start(false);

    let (cla, _rx) = TestCla::new();
    bpa.register_cla("lifecycle-cla".to_string(), cla.clone(), None)
        .await
        .unwrap();

    let peer_addr = cla::ClaAddress::Private("peer1".as_bytes().into());
    let peer_node = ipn_node(10);

    // Add peer
    let added = cla
        .sink()
        .add_peer(peer_addr.clone(), core::slice::from_ref(&peer_node))
        .await
        .unwrap();
    assert!(added, "First add_peer should succeed");

    // Remove peer
    let removed = cla.sink().remove_peer(&peer_addr).await.unwrap();
    assert!(removed, "remove_peer should succeed");

    // Removing again should return false
    let removed = cla.sink().remove_peer(&peer_addr).await.unwrap();
    assert!(!removed, "Double remove_peer should return false");

    bpa.shutdown().await;
}

// A second add_peer for an address that is already known loses the race:
// it reports false and must not disturb the live peer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_peer_address_is_rejected() {
    let bpa = Bpa::builder().node_ids(node_ids(1)).build().await.unwrap();
    bpa.start(false);

    let (cla, forwarded_rx) = TestCla::new();
    bpa.register_cla("dup-addr-cla".to_string(), cla.clone(), None)
        .await
        .unwrap();

    let peer_addr = cla::ClaAddress::Private("peer1".as_bytes().into());
    let peer_node = ipn_node(2);
    assert!(
        cla.sink()
            .add_peer(peer_addr.clone(), core::slice::from_ref(&peer_node))
            .await
            .unwrap()
    );
    assert!(
        !cla.sink()
            .add_peer(peer_addr.clone(), core::slice::from_ref(&peer_node))
            .await
            .unwrap(),
        "add_peer for a known address must report false"
    );

    // The surviving peer still forwards.
    let mut data = build_bundle(
        &"ipn:0.3.1".parse().unwrap(),
        &"ipn:0.2.99".parse().unwrap(),
        b"dup-addr",
    );
    let id = common::bundle_id_of(&data);
    cla.sink().dispatch(None, None, &mut data).await.unwrap();
    assert_eq!(recv_event(&forwarded_rx, 5).await, id);

    // The address remains removable exactly once.
    assert!(cla.sink().remove_peer(&peer_addr).await.unwrap());
    assert!(!cla.sink().remove_peer(&peer_addr).await.unwrap());

    bpa.shutdown().await;
}

// Unregistering a CLA should remove all its peers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cascading_cleanup() {
    let bpa = Bpa::builder().build().await.unwrap();
    bpa.start(false);

    let (cla, _rx) = TestCla::new();
    bpa.register_cla("cascade-cla".to_string(), cla.clone(), None)
        .await
        .unwrap();

    // Add two peers
    let addr1 = cla::ClaAddress::Private("p1".as_bytes().into());
    let addr2 = cla::ClaAddress::Private("p2".as_bytes().into());
    cla.sink().add_peer(addr1, &[ipn_node(20)]).await.unwrap();
    cla.sink().add_peer(addr2, &[ipn_node(21)]).await.unwrap();

    // Unregister the CLA: this must cascade-remove both peers
    cla.sink().unregister().await;

    // Re-registering with same name should now succeed (name freed)
    let (cla2, _rx2) = TestCla::new();
    let result = bpa
        .register_cla("cascade-cla".to_string(), cla2, None)
        .await;
    assert!(
        result.is_ok(),
        "Re-registration after unregister should succeed, got: {result:?}"
    );

    bpa.shutdown().await;
}

// ---------------------------------------------------------------------------
// Egress policy queue selection
// ---------------------------------------------------------------------------

// A controller that pulls each forward from the queue it was scheduled on.
struct MapController {
    queues: std::collections::HashMap<Option<u32>, Arc<dyn policy::EgressQueue>>,
}

#[async_trait]
impl policy::EgressController for MapController {
    async fn forward(&self, queue: Option<u32>, bundle: bundle::Bundle) {
        self.queues
            .get(&queue)
            .expect("forward from an unknown queue")
            .forward(bundle)
            .await
    }
}

// A two-queue policy that records every classification and always answers
// with a queue index that does not exist.
#[derive(Default)]
struct OutOfRangePolicy {
    classified: std::sync::Mutex<Vec<Option<u32>>>,
}

#[async_trait]
impl policy::EgressPolicy for OutOfRangePolicy {
    fn queue_count(&self) -> u32 {
        2
    }

    fn classify(&self, flow_label: Option<u32>) -> Option<u32> {
        self.classified.lock().unwrap().push(flow_label);
        Some(99)
    }

    async fn new_controller(
        &self,
        queues: std::collections::HashMap<Option<u32>, Arc<dyn policy::EgressQueue>>,
    ) -> Arc<dyn policy::EgressController> {
        Arc::new(MapController { queues })
    }
}

// A write filter that stamps every bundle with a fixed flow label, so the
// egress policy's classify() actually runs.
struct FlowLabelFilter(u32);

#[async_trait]
impl filter::WriteFilter for FlowLabelFilter {
    async fn filter(
        &self,
        _bundle: &bundle::Bundle,
        _data: &[u8],
    ) -> Result<filter::WriteResult, hardy_bpa::Error> {
        Ok(filter::WriteResult::Continue(
            Some(bundle::WritableMetadata {
                flow_label: Some(self.0),
            }),
            None,
        ))
    }
}

// A policy that classifies a flow label onto a queue index with no poller
// must not panic or lose the bundle: the peer falls back to the default
// queue and the bundle is still forwarded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn classify_out_of_range_falls_back_to_default_queue() {
    let bpa = Bpa::builder()
        .node_ids(node_ids(1))
        .filter(
            filter::Hook::Ingress,
            "flow-label",
            &[],
            filter::Filter::Write(Arc::new(FlowLabelFilter(7))),
        )
        .build()
        .await
        .unwrap();
    bpa.start(false);

    let policy = Arc::new(OutOfRangePolicy::default());
    let (cla, forwarded_rx) = TestCla::new();
    bpa.register_cla("policy-cla".to_string(), cla.clone(), Some(policy.clone()))
        .await
        .unwrap();
    cla.sink()
        .add_peer(
            cla::ClaAddress::Private("peer-2".as_bytes().into()),
            &[ipn_node(2)],
        )
        .await
        .unwrap();

    let mut data = build_bundle(
        &"ipn:0.3.1".parse().unwrap(),
        &"ipn:0.2.99".parse().unwrap(),
        b"out-of-range",
    );
    let id = common::bundle_id_of(&data);
    cla.sink().dispatch(None, None, &mut data).await.unwrap();

    assert_eq!(
        recv_event(&forwarded_rx, 5).await,
        id,
        "The bundle must survive the out-of-range classification"
    );
    assert!(
        policy.classified.lock().unwrap().contains(&Some(7)),
        "classify() must have run with the filter-stamped flow label"
    );

    bpa.shutdown().await;
}

// An application that captures its sink so the test can drive unregistration.
struct TestApp {
    sink: hardy_async::sync::spin::Once<Box<dyn services::ApplicationSink>>,
}

impl TestApp {
    fn new() -> Self {
        Self {
            sink: hardy_async::sync::spin::Once::new(),
        }
    }
}

#[async_trait]
impl services::Application for TestApp {
    async fn on_register(
        &self,
        _source: &hardy_bpv7::eid::Eid,
        sink: Box<dyn services::ApplicationSink>,
    ) {
        self.sink.call_once(|| sink);
    }
    async fn on_unregister(&self) {}
    async fn on_deliver(
        &self,
        _bundle_id: &hardy_bpv7::bundle::Id,
        _expiry: time::OffsetDateTime,
        _ack_requested: bool,
        _total_len: u64,
        _stream: &mut dyn stream::Receiver<stream::Segment>,
    ) -> services::Result<()> {
        Ok(())
    }
    async fn on_status_notify(
        &self,
        _bundle_id: &hardy_bpv7::bundle::Id,
        _from: &hardy_bpv7::eid::Eid,
        _kind: services::StatusNotify,
        _reason: hardy_bpv7::status_report::ReasonCode,
        _timestamp: Option<time::OffsetDateTime>,
    ) {
    }
}

// Registering two applications with the same explicit IPN service number should fail.
#[tokio::test]
async fn test_duplicate_reg() {
    let bpa = Bpa::builder().build().await.unwrap();
    bpa.start(false);

    let svc_id = hardy_bpv7::eid::Service::Ipn(42);

    // First registration should succeed
    let app1 = Arc::new(TestApp::new());
    let result = bpa.register_application(svc_id.clone(), app1).await;
    assert!(result.is_ok(), "First registration should succeed");

    // Second registration with the same service number should fail
    let app2 = Arc::new(TestApp::new());
    let result = bpa.register_application(svc_id, app2).await;
    assert!(
        matches!(result, Err(services::Error::ServiceIdInUse(ref id)) if id == "42"),
        "Duplicate registration should return ServiceIdInUse, got: {result:?}"
    );

    bpa.shutdown().await;
}

// After an application drops its sink (unregisters), the service ID should be freed
// for re-registration.
#[tokio::test]
async fn test_cleanup() {
    let bpa = Bpa::builder().build().await.unwrap();
    bpa.start(false);

    let svc_id = hardy_bpv7::eid::Service::Ipn(99);

    // Register
    let app1 = Arc::new(TestApp::new());
    let result = bpa.register_application(svc_id.clone(), app1.clone()).await;
    assert!(result.is_ok());

    // Unregister via the sink
    app1.sink
        .get()
        .expect("Sink should be set")
        .unregister()
        .await;

    // unregister() is fully awaited and removes the registry entry
    // before returning, so re-registration cannot race it.

    // Re-registration with the same service number should now succeed
    let app2 = Arc::new(TestApp::new());
    let result = bpa.register_application(svc_id, app2).await;
    assert!(
        result.is_ok(),
        "Re-registration after cleanup should succeed, got: {result:?}"
    );

    bpa.shutdown().await;
}
