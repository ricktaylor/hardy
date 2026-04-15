//! Application client proxy tests (APP-CLI-01 through APP-CLI-06).
//!
//! Verify the Application client correctly maps Rust trait calls
//! to service.proto messages (Application RPC) via the gRPC proxy.

mod common;

use common::MockBpa;
use hardy_bpa::async_trait;
use hardy_bpa::bpa::BpaRegistration;
use hardy_bpa::services::{Application, ApplicationSink, StatusNotify};
use hardy_bpv7::eid::Eid;
use hardy_proto::client::RemoteBpa;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

// A mock Application that records lifecycle callbacks and incoming payloads.
struct MockApplication {
    registered: AtomicBool,
    received: AtomicBool,
    status_notified: AtomicBool,
    sink: hardy_async::sync::spin::Mutex<Option<Box<dyn ApplicationSink>>>,
}

impl MockApplication {
    fn new() -> Self {
        Self {
            registered: AtomicBool::new(false),
            received: AtomicBool::new(false),
            status_notified: AtomicBool::new(false),
            sink: hardy_async::sync::spin::Mutex::new(None),
        }
    }

    fn take_sink(&self) -> Option<Box<dyn ApplicationSink>> {
        self.sink.lock().take()
    }
}

#[async_trait]
impl Application for MockApplication {
    async fn on_register(&self, _source: &Eid, sink: Box<dyn ApplicationSink>) {
        *self.sink.lock() = Some(sink);
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

// APP-CLI-01: Register application (IPN), receive endpoint ID.
#[tokio::test]
async fn app_cli_01_registration_ipn() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["application"]).await;

    let app = Arc::new(MockApplication::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let endpoint: Eid = remote_bpa
        .register_application(hardy_bpv7::eid::Service::Ipn(42), app.clone())
        .await
        .expect("registration should succeed");

    assert_eq!(endpoint.to_string(), "ipn:1.42");
    assert!(app.registered.load(Ordering::Relaxed));
    assert!(app.sink.lock().is_some());

    // Clean up
    drop(app.take_sink());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    server_tasks.shutdown().await;
}

// APP-CLI-02: Register application (DTN), receive endpoint ID.
#[tokio::test]
async fn app_cli_02_registration_dtn() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["application"]).await;

    let app = Arc::new(MockApplication::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    // MockBpa always returns ipn:1.42 regardless of service_id
    let endpoint: Eid = remote_bpa
        .register_application(hardy_bpv7::eid::Service::Dtn("sensor".into()), app.clone())
        .await
        .expect("registration should succeed");

    assert!(app.registered.load(Ordering::Relaxed));
    assert!(!endpoint.to_string().is_empty());

    // Clean up
    drop(app.take_sink());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    server_tasks.shutdown().await;
}

// APP-CLI-03: Send payload via sink.
#[tokio::test]
async fn app_cli_03_send_payload() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["application"]).await;

    let app = Arc::new(MockApplication::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _endpoint: Eid = remote_bpa
        .register_application(hardy_bpv7::eid::Service::Ipn(42), app.clone())
        .await
        .expect("registration should succeed");

    let sink = app.take_sink().expect("app should have a sink");

    // send() calls the mock BPA sink which returns an error
    let result = sink
        .send(
            "ipn:2.1".parse().unwrap(),
            hardy_bpa::Bytes::from_static(b"hello"),
            std::time::Duration::from_secs(3600),
            None,
        )
        .await;
    assert!(result.is_err(), "mock sink send returns error");

    // Clean up
    sink.unregister().await;
    server_tasks.shutdown().await;
}

// APP-CLI-04: Receive payload (BPA pushes to application).
#[tokio::test]
async fn app_cli_04_receive_payload() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["application"]).await;

    let app = Arc::new(MockApplication::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _endpoint: Eid = remote_bpa
        .register_application(hardy_bpv7::eid::Service::Ipn(42), app.clone())
        .await
        .expect("registration should succeed");

    assert!(!app.received.load(Ordering::Relaxed));

    let server_app = bpa
        .last_application
        .lock()
        .clone()
        .expect("BPA should have the server-side application");

    let source: Eid = "ipn:2.1".parse().unwrap();
    let expiry = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    server_app
        .on_receive(
            source,
            expiry,
            false,
            hardy_bpa::Bytes::from_static(b"hello"),
        )
        .await;

    assert!(
        app.received.load(Ordering::Relaxed),
        "MockApplication should have received the payload"
    );

    // Clean up
    drop(app.take_sink());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    server_tasks.shutdown().await;
}

// APP-CLI-05: Status notification (BPA pushes to application).
#[tokio::test]
async fn app_cli_05_status_notify() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["application"]).await;

    let app = Arc::new(MockApplication::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _endpoint: Eid = remote_bpa
        .register_application(hardy_bpv7::eid::Service::Ipn(42), app.clone())
        .await
        .expect("registration should succeed");

    assert!(!app.status_notified.load(Ordering::Relaxed));

    let server_app = bpa
        .last_application
        .lock()
        .clone()
        .expect("BPA should have the server-side application");

    let bundle_id = hardy_bpv7::bundle::Id {
        source: "ipn:1.42".parse().unwrap(),
        timestamp: hardy_bpv7::creation_timestamp::CreationTimestamp::new_sequential(),
        fragment_info: None,
    };
    let from: Eid = "ipn:2.0".parse().unwrap();

    server_app
        .on_status_notify(
            &bundle_id,
            &from,
            StatusNotify::Delivered,
            hardy_bpv7::status_report::ReasonCode::NoAdditionalInformation,
            None,
        )
        .await;

    assert!(
        app.status_notified.load(Ordering::Relaxed),
        "MockApplication should have received the status notification"
    );

    // Clean up
    drop(app.take_sink());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    server_tasks.shutdown().await;
}

// APP-CLI-06: Cancel pending send.
#[tokio::test]
async fn app_cli_06_cancel() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["application"]).await;

    let app = Arc::new(MockApplication::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _endpoint: Eid = remote_bpa
        .register_application(hardy_bpv7::eid::Service::Ipn(42), app.clone())
        .await
        .expect("registration should succeed");

    let sink = app.take_sink().expect("app should have a sink");

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
