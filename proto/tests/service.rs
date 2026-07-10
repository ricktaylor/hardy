//! Service client proxy tests (SVC-CLI-01 through SVC-CLI-06).
//!
//! Verify the low-level Service client correctly maps Rust trait calls
//! to service.proto messages via the gRPC proxy.

mod common;

use common::MockBpa;
use hardy_bpa::async_trait;
use hardy_bpa::bpa::BpaRegistration;
use hardy_bpa::services::{Service, ServiceSink, StatusNotify};
use hardy_bpv7::eid::Eid;
use hardy_proto::client::RemoteBpa;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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

    async fn on_receive(
        &self,
        _data: hardy_bpa::Bytes,
        _expiry: time::OffsetDateTime,
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
        .register_service(hardy_bpv7::eid::Service::Ipn(42), svc.clone())
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
        .register_service(hardy_bpv7::eid::Service::Ipn(42), svc.clone())
        .await
        .expect("registration should succeed");

    let sink = svc.take_sink().expect("service should have a sink");

    // send() calls the mock BPA sink which is unimplemented — expect an error
    let result = sink.send(hardy_bpa::Bytes::from_static(b"\x9f\x89")).await;
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
        .register_service(hardy_bpv7::eid::Service::Ipn(42), svc.clone())
        .await
        .expect("registration should succeed");

    assert!(!svc.received.load(Ordering::Relaxed));

    // BPA pushes a bundle via the server-side Service proxy
    let server_svc = bpa
        .last_service
        .lock()
        .clone()
        .expect("BPA should have the server-side service");

    let data = hardy_bpa::Bytes::from_static(b"\x9f\x89\x07\x00");
    let expiry = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    server_svc
        .on_receive(data, expiry)
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
        .register_service(hardy_bpv7::eid::Service::Ipn(42), svc.clone())
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

// SVC-CLI-05: Cancel pending send.
#[tokio::test]
async fn svc_cli_05_cancel() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["service"]).await;

    let svc = Arc::new(MockService::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _endpoint: Eid = remote_bpa
        .register_service(hardy_bpv7::eid::Service::Ipn(42), svc.clone())
        .await
        .expect("registration should succeed");

    let sink = svc.take_sink().expect("service should have a sink");

    let bundle_id = hardy_bpv7::bundle::Id {
        source: "ipn:1.42".parse().unwrap(),
        timestamp: hardy_bpv7::creation_timestamp::CreationTimestamp::new_sequential(),
        fragment_info: None,
    };

    let cancelled = sink
        .cancel(&bundle_id)
        .await
        .expect("cancel should succeed");

    assert!(cancelled, "bundle should be cancelled");

    // Clean up
    sink.unregister().await;
    server_tasks.shutdown().await;
}

// A service that replies from inside `on_receive`, the shape echo-service
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

    async fn on_receive(
        &self,
        data: hardy_bpa::Bytes,
        _expiry: time::OffsetDateTime,
    ) -> hardy_bpa::services::Result<()> {
        let sink = self.sink.lock().clone().expect("registered");
        // Send back to the BPA before returning. The mock sink answers with an
        // error, but the round-trip must complete: a Send drawn from the same
        // id space as the BPA's in-flight Receive used to be mis-routed as that
        // Receive's response, hanging this call forever.
        let _ = sink.send(data).await;
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
// from within `on_receive` must not wedge. The two ends of the proxy draw
// request ids from disjoint parities (proxy::Side), so a reply Send can never
// collide with the BPA's outstanding Receive. Before the fix this deadlocked
// deterministically on the first bundle.
#[tokio::test]
async fn svc_cli_06_concurrent_reply_from_on_receive() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["service"]).await;

    let replied = Arc::new(AtomicUsize::new(0));
    let svc = Arc::new(ReplyingService {
        sink: hardy_async::sync::spin::Mutex::new(None),
        replied: replied.clone(),
    });
    let remote_bpa = RemoteBpa::new(grpc_addr);
    remote_bpa
        .register_service(hardy_bpv7::eid::Service::Ipn(42), svc.clone())
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
            let _ = server_svc
                .on_receive(hardy_bpa::Bytes::from_static(b"payload"), expiry)
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
