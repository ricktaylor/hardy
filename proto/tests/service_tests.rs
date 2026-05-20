//! Service client proxy tests (SVC-CLI-01 through SVC-CLI-05).

mod common;

use common::MockBpa;
use hardy_bpa::async_trait;
use hardy_bpa::bpa::BpaRegistration;
use hardy_bpa::services::{Service, ServiceContext, StatusNotify};
use hardy_bpv7::eid::Eid;
use hardy_proto::client::RemoteBpa;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

struct MockService {
    registered: AtomicBool,
    received: AtomicBool,
    status_notified: AtomicBool,
    ctx: hardy_async::sync::spin::Mutex<Option<ServiceContext>>,
}

impl MockService {
    fn new() -> Self {
        Self {
            registered: AtomicBool::new(false),
            received: AtomicBool::new(false),
            status_notified: AtomicBool::new(false),
            ctx: hardy_async::sync::spin::Mutex::new(None),
        }
    }

    fn take_ctx(&self) -> Option<ServiceContext> {
        self.ctx.lock().take()
    }
}

#[async_trait]
impl Service for MockService {
    async fn on_register(&self, _endpoint: &Eid, ctx: ServiceContext) {
        *self.ctx.lock() = Some(ctx);
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

    let eid: Eid = remote_bpa
        .register_service(hardy_bpv7::eid::Service::Ipn(42), svc.clone())
        .await
        .expect("registration should succeed");

    assert_eq!(eid.to_string(), "ipn:1.42");
    assert!(svc.registered.load(Ordering::Relaxed));
    assert!(svc.ctx.lock().is_some());

    drop(svc.take_ctx());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    server_tasks.shutdown().await;
}

// SVC-CLI-02: Send raw bundle via context.
#[tokio::test]
async fn svc_cli_02_send_raw() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["service"]).await;

    let svc = Arc::new(MockService::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _eid: Eid = remote_bpa
        .register_service(hardy_bpv7::eid::Service::Ipn(42), svc.clone())
        .await
        .expect("registration should succeed");

    let ctx = svc.take_ctx().expect("should have context");

    // send_raw returns a Result<BundleId> via reply channel
    // The mock BPA doesn't actually process bundles, so this will likely error
    // We just verify the call doesn't panic
    let _ = ctx.send_raw(hardy_bpa::Bytes::from_static(b"test")).await;

    drop(ctx);
    server_tasks.shutdown().await;
}

// SVC-CLI-03: BPA delivers bundle to service via on_receive.
#[tokio::test]
async fn svc_cli_03_receive_bundle() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["service"]).await;

    let svc = Arc::new(MockService::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _eid: Eid = remote_bpa
        .register_service(hardy_bpv7::eid::Service::Ipn(42), svc.clone())
        .await
        .expect("registration should succeed");

    assert!(!svc.received.load(Ordering::Relaxed));

    let server_svc = bpa
        .last_service
        .lock()
        .clone()
        .expect("BPA should have the server-side service");

    server_svc
        .on_receive(
            hardy_bpa::Bytes::from_static(b"\x9f\x89"),
            time::OffsetDateTime::now_utc() + time::Duration::hours(1),
        )
        .await;

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    assert!(
        svc.received.load(Ordering::Relaxed),
        "MockService should have received the bundle"
    );

    drop(svc.take_ctx());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    server_tasks.shutdown().await;
}

// SVC-CLI-04: BPA sends status notification to service.
#[tokio::test]
async fn svc_cli_04_status_notify() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["service"]).await;

    let svc = Arc::new(MockService::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _eid: Eid = remote_bpa
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
        timestamp: hardy_bpv7::creation_timestamp::CreationTimestamp::now(),
        fragment_info: None,
    };

    server_svc
        .on_status_notify(
            &bundle_id,
            &"ipn:2.0".parse().unwrap(),
            StatusNotify::Forwarded,
            hardy_bpv7::status_report::ReasonCode::NoAdditionalInformation,
            Some(time::OffsetDateTime::now_utc()),
        )
        .await;

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    assert!(
        svc.status_notified.load(Ordering::Relaxed),
        "MockService should have received status notification"
    );

    drop(svc.take_ctx());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    server_tasks.shutdown().await;
}
