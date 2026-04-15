//! Service client proxy tests (SVC-CLI-01 through SVC-CLI-05).
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
use std::sync::atomic::{AtomicBool, Ordering};

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

    async fn on_receive(&self, _data: hardy_bpa::Bytes, _expiry: time::OffsetDateTime) {
        self.received.store(true, Ordering::Relaxed);
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
        .register_service(Some(hardy_bpv7::eid::Service::Ipn(42)), svc.clone())
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
        .register_service(Some(hardy_bpv7::eid::Service::Ipn(42)), svc.clone())
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
        .register_service(Some(hardy_bpv7::eid::Service::Ipn(42)), svc.clone())
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
    server_svc.on_receive(data, expiry).await;

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
        .register_service(Some(hardy_bpv7::eid::Service::Ipn(42)), svc.clone())
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
        .register_service(Some(hardy_bpv7::eid::Service::Ipn(42)), svc.clone())
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
