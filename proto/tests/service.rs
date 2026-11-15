//! Service client proxy tests (SVC-CLI-01 through SVC-CLI-08).
//!
//! Verify the low-level Service client correctly maps Rust trait calls
//! to service.proto messages via the gRPC proxy.

mod common;

use core::{num::NonZeroU32, time::Duration};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use hardy_bpa::{
    Bytes, async_trait,
    bpa::{Bpa, BpaRegistration},
    lanes::Lanes,
    node_ids::NodeIds,
    services::{Application, ApplicationSink, Service, ServiceSink, StatusNotify},
    stream::{Receiver, Segment, buffer_stream},
};
use hardy_bpv7::{
    bundle::Id,
    eid::{Eid, IpnNodeId, NodeId, Service as ServiceId},
    status_report::ReasonCode,
};
use hardy_proto::client::RemoteBpa;
use time::OffsetDateTime;
use tokio::sync::{Semaphore, mpsc};

use common::MockBpa;

// A mock Service that records lifecycle callbacks and incoming bundles.
struct MockService {
    registered: AtomicBool,
    received: AtomicBool,
    status_notified: AtomicBool,
    sink: hardy_async::sync::spin::Mutex<Option<Box<dyn ServiceSink>>>,
}

impl MockService {
    fn new() -> Self {
        Self {
            registered: AtomicBool::new(false),
            received: AtomicBool::new(false),
            status_notified: AtomicBool::new(false),
            sink: hardy_async::sync::spin::Mutex::new(None),
        }
    }

    fn take_sink(&self) -> Option<Box<dyn ServiceSink>> {
        self.sink.lock().take()
    }
}

#[async_trait]
impl Service for MockService {
    async fn on_register(&self, _endpoint: &Eid, sink: Box<dyn ServiceSink>) {
        *self.sink.lock() = Some(sink);
        self.registered.store(true, Ordering::Relaxed);
    }

    async fn on_unregister(&self) {}

    async fn on_deliver(
        &self,
        _bundle_id: &hardy_bpv7::bundle::Id,
        _expiry: time::OffsetDateTime,
        _total_len: u64,
        _stream: &mut dyn hardy_bpa::stream::Receiver<hardy_bpa::stream::Segment>,
    ) -> hardy_bpa::services::Result<()> {
        self.received.store(true, Ordering::Relaxed);
        Ok(())
    }

    async fn on_status_notify(
        &self,
        _bundle_id: &hardy_bpv7::bundle::Id,
        _from: &Eid,
        _kind: StatusNotify,
        _reason: hardy_bpv7::status_report::ReasonCode,
        _timestamp: Option<time::OffsetDateTime>,
    ) {
        self.status_notified.store(true, Ordering::Relaxed);
    }
}

// SVC-CLI-01: Register service, receive endpoint ID.
#[tokio::test]
async fn svc_cli_01_registration() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["service"]).await;

    let svc = Arc::new(MockService::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let endpoint: Eid = remote_bpa
        .register_service(
            hardy_bpv7::eid::Service::Ipn(42),
            svc.clone(),
            Lanes::Serial,
        )
        .await
        .expect("registration should succeed");

    assert_eq!(endpoint.to_string(), "ipn:1.42");
    assert!(svc.registered.load(Ordering::Relaxed));
    assert!(svc.sink.lock().is_some());

    // Clean up
    drop(svc.take_sink());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    server_tasks.shutdown().await;
}

// SVC-CLI-02: Send raw bundle via sink.
#[tokio::test]
async fn svc_cli_02_send_bundle() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["service"]).await;

    let svc = Arc::new(MockService::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _endpoint: Eid = remote_bpa
        .register_service(
            hardy_bpv7::eid::Service::Ipn(42),
            svc.clone(),
            Lanes::Serial,
        )
        .await
        .expect("registration should succeed");

    let sink = svc.take_sink().expect("service should have a sink");

    // send() calls the mock BPA sink which is unimplemented — expect an error
    let result = sink
        .send(&mut hardy_bpa::Bytes::from_static(b"\x9f\x89"))
        .await;
    assert!(result.is_err(), "mock sink send is unimplemented");

    // Clean up
    sink.unregister().await;
    server_tasks.shutdown().await;
}

// SVC-CLI-03: Receive raw bundle (BPA pushes to service).
#[tokio::test]
async fn svc_cli_03_receive_bundle() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["service"]).await;

    let svc = Arc::new(MockService::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _endpoint: Eid = remote_bpa
        .register_service(
            hardy_bpv7::eid::Service::Ipn(42),
            svc.clone(),
            Lanes::Serial,
        )
        .await
        .expect("registration should succeed");

    assert!(!svc.received.load(Ordering::Relaxed));

    // BPA pushes a bundle via the server-side Service proxy
    let server_svc = bpa
        .last_service
        .lock()
        .clone()
        .expect("BPA should have the server-side service");

    let mut data = hardy_bpa::Bytes::from_static(b"\x9f\x89\x07\x00");
    let total_len = data.len() as u64;
    let bundle_id = hardy_bpv7::bundle::Id::default();
    let expiry = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    server_svc
        .on_deliver(&bundle_id, expiry, total_len, &mut data)
        .await
        .expect("Delivery should succeed");

    assert!(
        svc.received.load(Ordering::Relaxed),
        "MockService should have received the bundle"
    );

    // Clean up
    drop(svc.take_sink());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    server_tasks.shutdown().await;
}

// SVC-CLI-04: Status notification (BPA pushes to service).
#[tokio::test]
async fn svc_cli_04_status_notify() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["service"]).await;

    let svc = Arc::new(MockService::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _endpoint: Eid = remote_bpa
        .register_service(
            hardy_bpv7::eid::Service::Ipn(42),
            svc.clone(),
            Lanes::Serial,
        )
        .await
        .expect("registration should succeed");

    assert!(!svc.status_notified.load(Ordering::Relaxed));

    let server_svc = bpa
        .last_service
        .lock()
        .clone()
        .expect("BPA should have the server-side service");

    let bundle_id = hardy_bpv7::bundle::Id {
        source: "ipn:1.42".parse().unwrap(),
        timestamp: hardy_bpv7::creation_timestamp::CreationTimestamp::new_sequential(),
        fragment_info: None,
    };
    let from: Eid = "ipn:2.0".parse().unwrap();

    server_svc
        .on_status_notify(
            &bundle_id,
            &from,
            StatusNotify::Delivered,
            hardy_bpv7::status_report::ReasonCode::NoAdditionalInformation,
            None,
        )
        .await;

    assert!(
        svc.status_notified.load(Ordering::Relaxed),
        "MockService should have received the status notification"
    );

    // Clean up
    drop(svc.take_sink());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    server_tasks.shutdown().await;
}

// A service that replies from inside `on_deliver`, the shape echo-service
// and any request/reply service uses.
struct ReplyingService {
    sink: hardy_async::sync::spin::Mutex<Option<Arc<dyn ServiceSink>>>,
    replied: Arc<AtomicUsize>,
}

#[async_trait]
impl Service for ReplyingService {
    async fn on_register(&self, _endpoint: &Eid, sink: Box<dyn ServiceSink>) {
        *self.sink.lock() = Some(Arc::from(sink));
    }

    async fn on_unregister(&self) {}

    async fn on_deliver(
        &self,
        _bundle_id: &hardy_bpv7::bundle::Id,
        _expiry: time::OffsetDateTime,
        total_len: u64,
        stream: &mut dyn hardy_bpa::stream::Receiver<hardy_bpa::stream::Segment>,
    ) -> hardy_bpa::services::Result<()> {
        let mut data = hardy_bpa::stream::buffer_stream(stream, total_len).await?;
        let sink = self.sink.lock().clone().expect("registered");
        // Send back to the BPA before returning. The mock sink answers with an
        // error, but the round-trip must complete: a Send drawn from the same
        // id space as the BPA's in-flight Receive used to be mis-routed as that
        // Receive's response, hanging this call forever.
        let _ = sink.send(&mut data).await;
        self.replied.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    async fn on_status_notify(
        &self,
        _bundle_id: &hardy_bpv7::bundle::Id,
        _from: &Eid,
        _kind: StatusNotify,
        _reason: hardy_bpv7::status_report::ReasonCode,
        _timestamp: Option<time::OffsetDateTime>,
    ) {
    }
}

// SVC-CLI-06 (regression): concurrent deliveries to a service that replies
// from within `on_deliver` must not wedge. The two ends of the proxy draw
// request ids from disjoint parities (proxy::Side), so a reply Send can never
// collide with the BPA's outstanding Receive. Before the fix this deadlocked
// deterministically on the first bundle.
#[tokio::test]
async fn svc_cli_06_concurrent_reply_from_on_deliver() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["service"]).await;

    let replied = Arc::new(AtomicUsize::new(0));
    let svc = Arc::new(ReplyingService {
        sink: hardy_async::sync::spin::Mutex::new(None),
        replied: replied.clone(),
    });
    let remote_bpa = RemoteBpa::new(grpc_addr);
    remote_bpa
        .register_service(
            hardy_bpv7::eid::Service::Ipn(42),
            svc.clone(),
            Lanes::Serial,
        )
        .await
        .expect("registration should succeed");

    let server_svc = bpa
        .last_service
        .lock()
        .clone()
        .expect("BPA should have the server-side service");

    // Drive more concurrent deliveries than the handler pool has permits, so a
    // wedge (leaked permits) cannot be masked by spare capacity.
    const N: usize = 32;
    let expiry = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    let deliveries = (0..N).map(|_| {
        let server_svc = server_svc.clone();
        tokio::spawn(async move {
            let mut data = hardy_bpa::Bytes::from_static(b"payload");
            let total_len = data.len() as u64;
            let _ = server_svc
                .on_deliver(
                    &hardy_bpv7::bundle::Id::default(),
                    expiry,
                    total_len,
                    &mut data,
                )
                .await;
        })
    });

    let drive = async {
        for d in deliveries {
            let _ = d.await;
        }
        while replied.load(Ordering::Relaxed) < N {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), drive)
        .await
        .expect("concurrent replying deliveries must not wedge");

    assert_eq!(replied.load(Ordering::Relaxed), N);

    // Close the client so the server's bidi stream ends; otherwise
    // server_tasks.shutdown() waits on the still-open stream.
    let sink = svc.sink.lock().take();
    if let Some(sink) = sink {
        sink.unregister().await;
    }
    server_tasks.shutdown().await;
}

// SVC-CLI-07: the delivery lanes round-trip the wire: limited and
// unlimited declarations reach the BPA's register_service intact, and a
// service declaring nothing reaches it as serial (absent on the wire).
#[tokio::test]
async fn svc_cli_07_lanes_round_trip() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["service"]).await;
    let remote_bpa = RemoteBpa::new(grpc_addr);

    for lanes in [
        Lanes::Limited(NonZeroU32::new(6).unwrap()),
        Lanes::Unlimited,
        Lanes::Serial,
    ] {
        let svc = Arc::new(MockService::new());
        remote_bpa
            .register_service(ServiceId::Ipn(42), svc.clone(), lanes)
            .await
            .expect("registration should succeed");
        assert_eq!(
            *bpa.declared_service_lanes.lock(),
            Some(lanes),
            "the BPA should receive the service's declarations intact"
        );
        if let Some(sink) = svc.take_sink() {
            sink.unregister().await;
        }
    }

    server_tasks.shutdown().await;
}

// A remote service whose deliveries announce themselves and hold until the
// test releases them, recording the most ever in flight at once.
struct HoldingService {
    sink: hardy_async::sync::spin::Mutex<Option<Box<dyn ServiceSink>>>,
    entered_tx: mpsc::UnboundedSender<()>,
    release: Arc<Semaphore>,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
}

#[async_trait]
impl Service for HoldingService {
    async fn on_register(&self, _endpoint: &Eid, sink: Box<dyn ServiceSink>) {
        *self.sink.lock() = Some(sink);
    }

    async fn on_unregister(&self) {}

    async fn on_deliver(
        &self,
        _bundle_id: &Id,
        _expiry: OffsetDateTime,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> hardy_bpa::services::Result<()> {
        buffer_stream(stream, total_len).await?;
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(now, Ordering::SeqCst);
        let _ = self.entered_tx.send(());
        self.release
            .acquire()
            .await
            .expect("release semaphore open")
            .forget();
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(())
    }

    async fn on_status_notify(
        &self,
        _bundle_id: &Id,
        _from: &Eid,
        _kind: StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<OffsetDateTime>,
    ) {
    }
}

// A local application on the BPA, originating bundles to the remote
// service.
struct Originator {
    sink: hardy_async::sync::spin::Once<Box<dyn ApplicationSink>>,
}

#[async_trait]
impl Application for Originator {
    async fn on_register(&self, _source: &Eid, sink: Box<dyn ApplicationSink>) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn on_deliver(
        &self,
        _bundle_id: &Id,
        _expiry: OffsetDateTime,
        _ack_requested: bool,
        _total_len: u64,
        _stream: &mut dyn Receiver<Segment>,
    ) -> hardy_bpa::services::Result<()> {
        Ok(())
    }

    async fn on_status_notify(
        &self,
        _bundle_id: &Id,
        _from: &Eid,
        _kind: StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<OffsetDateTime>,
    ) {
    }
}

// Event-driven wait for the next delivery to enter the service; the timeout
// only bounds a regression.
async fn entered(rx: &mut mpsc::UnboundedReceiver<()>) {
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("delivery never arrived")
        .expect("service gone")
}

// SVC-CLI-08: end to end against a real BPA, a remote service's declared
// lane count reaches the server proxy's registration and sets its
// effective delivery lanes, and the client's proxy makes room for them
// whatever the host's core count: with more lanes declared than the host
// has cores (and the delivery cap raised to match), that many deliveries
// are in flight across the wire at once, never more, and the next starts
// when a lane frees.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn svc_cli_08_declared_lanes_deliver_concurrently_end_to_end() {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let lanes = hardy_async::available_parallelism().get() + 2;
    let lane_count = NonZeroU32::new(u32::try_from(lanes).unwrap()).unwrap();
    let bpa = Arc::new(
        Bpa::builder()
            .node_ids(node_ids)
            .delivery_lane_cap(lane_count)
            .build()
            .await
            .unwrap(),
    );
    bpa.start(false).await;
    let (grpc_addr, server_tasks) = common::start_server_for(bpa.clone(), &["service"]).await;

    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let release = Arc::new(Semaphore::new(0));
    let svc = Arc::new(HoldingService {
        sink: hardy_async::sync::spin::Mutex::new(None),
        entered_tx,
        release: release.clone(),
        in_flight: AtomicUsize::new(0),
        max_in_flight: AtomicUsize::new(0),
    });
    RemoteBpa::new(grpc_addr)
        .register_service(ServiceId::Ipn(42), svc.clone(), Lanes::Limited(lane_count))
        .await
        .expect("registration should succeed");

    let originator = Arc::new(Originator {
        sink: hardy_async::sync::spin::Once::new(),
    });
    bpa.register_application(ServiceId::Ipn(9), originator.clone(), Lanes::Serial)
        .await
        .unwrap();
    for _ in 0..=lanes {
        originator
            .sink
            .get()
            .unwrap()
            .send(
                "ipn:0.1.42".parse().unwrap(),
                Bytes::from_static(b"lanes over the wire"),
                Duration::from_secs(3600),
                None,
            )
            .await
            .expect("origination should succeed");
    }

    for _ in 0..lanes {
        entered(&mut entered_rx).await;
    }
    assert_eq!(svc.in_flight.load(Ordering::SeqCst), lanes);

    release.add_permits(1);
    entered(&mut entered_rx).await;

    release.add_permits(lanes);
    let sink = svc.sink.lock().take();
    if let Some(sink) = sink {
        sink.unregister().await;
    }
    server_tasks.shutdown().await;
    bpa.shutdown().await;

    assert_eq!(
        svc.max_in_flight.load(Ordering::SeqCst),
        lanes,
        "the declared lanes bound the deliveries in flight"
    );
}
