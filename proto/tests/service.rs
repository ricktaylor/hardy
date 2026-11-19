//! Service client proxy tests (SVC-CLI-01 through SVC-CLI-06).
//!
//! Verify the low-level Service client correctly maps Rust trait calls
//! to service.proto messages via the gRPC proxy.

mod common;

use common::MockBpa;
use hardy_bpa::async_trait;
use hardy_bpa::service::{RegistrarExt, StatusNotify, raw};
use hardy_bpv7::eid::Eid;
use hardy_proto::client::RemoteBpa;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

// A mock Service that records the context it was built from and the
// bundles the BPA pushes at it.
struct MockService {
    sink: Arc<dyn raw::Sink>,
    endpoint: Eid,
    received: AtomicBool,
    status_notified: AtomicBool,
}

impl MockService {
    fn factory() -> impl raw::Factory<Component = Self> {
        raw::factory_fn(|cx: raw::Context| {
            Ok(Arc::new(Self {
                sink: cx.sink,
                endpoint: cx.endpoint,
                received: AtomicBool::new(false),
                status_notified: AtomicBool::new(false),
            }))
        })
    }
}

#[async_trait]
impl raw::Service for MockService {
    async fn deliver(
        &self,
        _bundle_id: &hardy_bpv7::bundle::Id,
        _expiry: time::OffsetDateTime,
        _total_len: u64,
        _stream: &mut dyn hardy_bpa::stream::Receiver<hardy_bpa::stream::Segment>,
    ) -> hardy_bpa::service::Result<()> {
        self.received.store(true, Ordering::Relaxed);
        Ok(())
    }

    async fn report_status(
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

    let service = RemoteBpa::new(grpc_addr)
        .register_raw_service(
            Some(hardy_bpv7::eid::Service::Ipn(42)),
            MockService::factory(),
        )
        .await
        .expect("registration should succeed");

    assert_eq!(service.endpoint.to_string(), "ipn:1.42");

    // Clean up
    service.sink.unregister().await;
    server_tasks.shutdown().await;
}

// SVC-CLI-02: Send raw bundle via sink.
#[tokio::test]
async fn svc_cli_02_send_bundle() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["service"]).await;

    let service = RemoteBpa::new(grpc_addr)
        .register_raw_service(
            Some(hardy_bpv7::eid::Service::Ipn(42)),
            MockService::factory(),
        )
        .await
        .expect("registration should succeed");

    // send() calls the mock BPA sink which is unimplemented — expect an error
    let result = service
        .sink
        .send(&mut hardy_bpa::Bytes::from_static(b"\x9f\x89"))
        .await;
    assert!(result.is_err(), "mock sink send is unimplemented");

    // Clean up
    service.sink.unregister().await;
    server_tasks.shutdown().await;
}

// SVC-CLI-03: Receive raw bundle (BPA pushes to service).
#[tokio::test]
async fn svc_cli_03_receive_bundle() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["service"]).await;

    let service = RemoteBpa::new(grpc_addr)
        .register_raw_service(
            Some(hardy_bpv7::eid::Service::Ipn(42)),
            MockService::factory(),
        )
        .await
        .expect("registration should succeed");

    assert!(!service.received.load(Ordering::Relaxed));

    // BPA pushes a bundle via the server-side Service proxy
    let server_service = bpa
        .last_raw_service
        .lock()
        .clone()
        .expect("BPA should have the server-side service");

    let mut data = hardy_bpa::Bytes::from_static(b"\x9f\x89\x07\x00");
    let total_len = data.len() as u64;
    let bundle_id = hardy_bpv7::bundle::Id::default();
    let expiry = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    server_service
        .deliver(&bundle_id, expiry, total_len, &mut data)
        .await
        .expect("Delivery should succeed");

    assert!(
        service.received.load(Ordering::Relaxed),
        "MockService should have received the bundle"
    );

    // Clean up
    service.sink.unregister().await;
    server_tasks.shutdown().await;
}

// SVC-CLI-04: Status notification (BPA pushes to service).
#[tokio::test]
async fn svc_cli_04_status_notify() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["service"]).await;

    let service = RemoteBpa::new(grpc_addr)
        .register_raw_service(
            Some(hardy_bpv7::eid::Service::Ipn(42)),
            MockService::factory(),
        )
        .await
        .expect("registration should succeed");

    assert!(!service.status_notified.load(Ordering::Relaxed));

    let server_service = bpa
        .last_raw_service
        .lock()
        .clone()
        .expect("BPA should have the server-side service");

    let bundle_id = hardy_bpv7::bundle::Id {
        source: "ipn:1.42".parse().unwrap(),
        timestamp: hardy_bpv7::creation_timestamp::CreationTimestamp::new_sequential(),
        fragment_info: None,
    };
    let from: Eid = "ipn:2.0".parse().unwrap();

    server_service
        .report_status(
            &bundle_id,
            &from,
            StatusNotify::Delivered,
            hardy_bpv7::status_report::ReasonCode::NoAdditionalInformation,
            None,
        )
        .await;

    assert!(
        service.status_notified.load(Ordering::Relaxed),
        "MockService should have received the status notification"
    );

    // Clean up
    service.sink.unregister().await;
    server_tasks.shutdown().await;
}

// A service that replies from inside `deliver`, the shape echo-service
// and any request/reply service uses.
struct ReplyingService {
    sink: Arc<dyn raw::Sink>,
    replied: Arc<AtomicUsize>,
}

impl ReplyingService {
    fn factory(replied: Arc<AtomicUsize>) -> impl raw::Factory<Component = Self> {
        raw::factory_fn(move |cx: raw::Context| {
            Ok(Arc::new(Self {
                sink: cx.sink,
                replied,
            }))
        })
    }
}

#[async_trait]
impl raw::Service for ReplyingService {
    async fn deliver(
        &self,
        _bundle_id: &hardy_bpv7::bundle::Id,
        _expiry: time::OffsetDateTime,
        total_len: u64,
        stream: &mut dyn hardy_bpa::stream::Receiver<hardy_bpa::stream::Segment>,
    ) -> hardy_bpa::service::Result<()> {
        let mut data = hardy_bpa::stream::buffer_stream(stream, total_len).await?;
        // Send back to the BPA before returning. The mock sink answers with an
        // error, but the round-trip must complete: a Send drawn from the same
        // id space as the BPA's in-flight Receive used to be mis-routed as that
        // Receive's response, hanging this call forever.
        let _ = self.sink.send(&mut data).await;
        self.replied.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    async fn report_status(
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
// from within `deliver` must not wedge. The two ends of the proxy draw
// request ids from disjoint parities (proxy::Side), so a reply Send can never
// collide with the BPA's outstanding Receive. Before the fix this deadlocked
// deterministically on the first bundle.
#[tokio::test]
async fn svc_cli_06_concurrent_reply_from_deliver() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["service"]).await;

    let replied = Arc::new(AtomicUsize::new(0));
    let service = RemoteBpa::new(grpc_addr)
        .register_raw_service(
            Some(hardy_bpv7::eid::Service::Ipn(42)),
            ReplyingService::factory(replied.clone()),
        )
        .await
        .expect("registration should succeed");

    let server_service = bpa
        .last_raw_service
        .lock()
        .clone()
        .expect("BPA should have the server-side service");

    // Drive more concurrent deliveries than the handler pool has permits, so a
    // wedge (leaked permits) cannot be masked by spare capacity.
    const N: usize = 32;
    let expiry = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    let deliveries = (0..N).map(|_| {
        let server_service = server_service.clone();
        tokio::spawn(async move {
            let mut data = hardy_bpa::Bytes::from_static(b"payload");
            let total_len = data.len() as u64;
            let _ = server_service
                .deliver(
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
    service.sink.unregister().await;
    server_tasks.shutdown().await;
}
