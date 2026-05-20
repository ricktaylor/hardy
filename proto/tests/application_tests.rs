//! Application client proxy tests (APP-CLI-01 through APP-CLI-06).

mod common;

use common::MockBpa;
use hardy_bpa::async_trait;
use hardy_bpa::bpa::BpaRegistration;
use hardy_bpa::services::{AppContext, Application, StatusNotify};
use hardy_bpv7::eid::Eid;
use hardy_proto::client::RemoteBpa;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

struct MockApplication {
    registered: AtomicBool,
    received: AtomicBool,
    status_notified: AtomicBool,
    ctx: hardy_async::sync::spin::Mutex<Option<AppContext>>,
}

impl MockApplication {
    fn new() -> Self {
        Self {
            registered: AtomicBool::new(false),
            received: AtomicBool::new(false),
            status_notified: AtomicBool::new(false),
            ctx: hardy_async::sync::spin::Mutex::new(None),
        }
    }

    fn take_ctx(&self) -> Option<AppContext> {
        self.ctx.lock().take()
    }
}

#[async_trait]
impl Application for MockApplication {
    async fn on_register(&self, _source: &Eid, ctx: AppContext) {
        *self.ctx.lock() = Some(ctx);
        self.registered.store(true, Ordering::Relaxed);
    }

    async fn on_unregister(&self) {}

    async fn on_receive(
        &self,
        _source: Eid,
        _expiry: time::OffsetDateTime,
        _ack_requested: bool,
        _payload: hardy_bpa::Bytes,
    ) {
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

// APP-CLI-01: Register application, receive endpoint ID.
#[tokio::test]
async fn app_cli_01_registration() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["application"]).await;

    let app = Arc::new(MockApplication::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let eid: Eid = remote_bpa
        .register_application(hardy_bpv7::eid::Service::Ipn(42), app.clone())
        .await
        .expect("registration should succeed");

    assert_eq!(eid.to_string(), "ipn:1.42");
    assert!(app.registered.load(Ordering::Relaxed));
    assert!(app.ctx.lock().is_some());

    drop(app.take_ctx());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    server_tasks.shutdown().await;
}

// APP-CLI-02: Send payload via context.
#[tokio::test]
async fn app_cli_02_send_payload() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["application"]).await;

    let app = Arc::new(MockApplication::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _eid: Eid = remote_bpa
        .register_application(hardy_bpv7::eid::Service::Ipn(42), app.clone())
        .await
        .expect("registration should succeed");

    let ctx = app.take_ctx().expect("should have context");

    let _ = ctx
        .send(
            "ipn:2.0".parse().unwrap(),
            hardy_bpa::Bytes::from_static(b"test"),
            core::time::Duration::from_secs(3600),
            None,
        )
        .await;

    drop(ctx);
    server_tasks.shutdown().await;
}

// APP-CLI-03: BPA delivers payload to application.
#[tokio::test]
async fn app_cli_03_receive_payload() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["application"]).await;

    let app = Arc::new(MockApplication::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _eid: Eid = remote_bpa
        .register_application(hardy_bpv7::eid::Service::Ipn(42), app.clone())
        .await
        .expect("registration should succeed");

    assert!(!app.received.load(Ordering::Relaxed));

    let server_app = bpa
        .last_application
        .lock()
        .clone()
        .expect("BPA should have the server-side app");

    server_app
        .on_receive(
            "ipn:2.0".parse().unwrap(),
            time::OffsetDateTime::now_utc() + time::Duration::hours(1),
            false,
            hardy_bpa::Bytes::from_static(b"hello"),
        )
        .await;

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    assert!(
        app.received.load(Ordering::Relaxed),
        "MockApplication should have received the payload"
    );

    drop(app.take_ctx());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    server_tasks.shutdown().await;
}

// APP-CLI-04: BPA sends status notification to application.
#[tokio::test]
async fn app_cli_04_status_notify() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["application"]).await;

    let app = Arc::new(MockApplication::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _eid: Eid = remote_bpa
        .register_application(hardy_bpv7::eid::Service::Ipn(42), app.clone())
        .await
        .expect("registration should succeed");

    assert!(!app.status_notified.load(Ordering::Relaxed));

    let server_app = bpa
        .last_application
        .lock()
        .clone()
        .expect("BPA should have the server-side app");

    let bundle_id = hardy_bpv7::bundle::Id {
        source: "ipn:1.42".parse().unwrap(),
        timestamp: hardy_bpv7::creation_timestamp::CreationTimestamp::now(),
        fragment_info: None,
    };

    server_app
        .on_status_notify(
            &bundle_id,
            &"ipn:2.0".parse().unwrap(),
            StatusNotify::Delivered,
            hardy_bpv7::status_report::ReasonCode::NoAdditionalInformation,
            Some(time::OffsetDateTime::now_utc()),
        )
        .await;

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    assert!(
        app.status_notified.load(Ordering::Relaxed),
        "MockApplication should have received status notification"
    );

    drop(app.take_ctx());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    server_tasks.shutdown().await;
}
