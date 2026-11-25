//! Application client proxy tests (APP-CLI-01 through APP-CLI-05).
//!
//! Verify the Application client correctly maps Rust trait calls
//! to service.proto messages (Application RPC) via the gRPC proxy.

mod common;

use common::MockBpa;
use hardy_bpa::async_trait;
use hardy_bpa::bpa::BpaRegistration;
use hardy_bpa::service::{Application, ApplicationSink, StatusNotify};
use hardy_bpv7::eid::Eid;
use hardy_proto::client::RemoteBpa;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

// A mock service that records lifecycle callbacks and incoming payloads.
struct MockService {
    registered: AtomicBool,
    received: AtomicBool,
    received_id: hardy_async::sync::spin::Mutex<Option<hardy_bpv7::bundle::Id>>,
    status_notified: AtomicBool,
    sink: hardy_async::sync::spin::Mutex<Option<Box<dyn ApplicationSink>>>,
}

impl MockService {
    fn new() -> Self {
        Self {
            registered: AtomicBool::new(false),
            received: AtomicBool::new(false),
            received_id: hardy_async::sync::spin::Mutex::new(None),
            status_notified: AtomicBool::new(false),
            sink: hardy_async::sync::spin::Mutex::new(None),
        }
    }

    fn take_sink(&self) -> Option<Box<dyn ApplicationSink>> {
        self.sink.lock().take()
    }
}

#[async_trait]
impl Application for MockService {
    async fn on_register(&self, _source: &Eid, sink: Box<dyn ApplicationSink>) {
        *self.sink.lock() = Some(sink);
        self.registered.store(true, Ordering::Relaxed);
    }

    async fn on_unregister(&self) {}

    async fn on_deliver(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
        _expiry: time::OffsetDateTime,
        _ack_requested: bool,
        _total_len: u64,
        _stream: &mut dyn hardy_bpa::stream::Receiver<hardy_bpa::stream::Segment>,
    ) -> hardy_bpa::service::Result<()> {
        *self.received_id.lock() = Some(bundle_id.clone());
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

// APP-CLI-01: Register application (IPN), receive endpoint ID.
#[tokio::test]
async fn app_cli_01_registration_ipn() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["application"]).await;

    let service = Arc::new(MockService::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let endpoint: Eid = remote_bpa
        .register_application(hardy_bpv7::eid::Service::Ipn(42), service.clone())
        .await
        .expect("registration should succeed");

    assert_eq!(endpoint.to_string(), "ipn:1.42");
    assert!(service.registered.load(Ordering::Relaxed));
    assert!(service.sink.lock().is_some());

    // Clean up
    drop(service.take_sink());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    server_tasks.shutdown().await;
}

// APP-CLI-02: Register application (DTN), receive endpoint ID.
#[tokio::test]
async fn app_cli_02_registration_dtn() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["application"]).await;

    let service = Arc::new(MockService::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    // MockBpa always returns ipn:1.42 regardless of service_id
    let endpoint: Eid = remote_bpa
        .register_application(
            hardy_bpv7::eid::Service::Dtn("sensor".into()),
            service.clone(),
        )
        .await
        .expect("registration should succeed");

    assert!(service.registered.load(Ordering::Relaxed));
    assert!(!endpoint.to_string().is_empty());

    // Clean up
    drop(service.take_sink());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    server_tasks.shutdown().await;
}

// APP-CLI-03: Send payload via sink.
#[tokio::test]
async fn app_cli_03_send_payload() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["application"]).await;

    let service = Arc::new(MockService::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _endpoint: Eid = remote_bpa
        .register_application(hardy_bpv7::eid::Service::Ipn(42), service.clone())
        .await
        .expect("registration should succeed");

    let sink = service.take_sink().expect("service should have a sink");

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

    let service = Arc::new(MockService::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _endpoint: Eid = remote_bpa
        .register_application(hardy_bpv7::eid::Service::Ipn(42), service.clone())
        .await
        .expect("registration should succeed");

    assert!(!service.received.load(Ordering::Relaxed));

    let server_service = bpa
        .last_application
        .lock()
        .clone()
        .expect("BPA should have the server-side application");

    let source: Eid = "ipn:2.1".parse().unwrap();
    let bundle_id = hardy_bpv7::bundle::Id {
        source: source.clone(),
        ..Default::default()
    };
    let expiry = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    let mut payload = hardy_bpa::Bytes::from_static(b"hello");
    let total_len = payload.len() as u64;
    server_service
        .on_deliver(&bundle_id, expiry, false, total_len, &mut payload)
        .await
        .expect("Delivery should succeed");

    assert!(
        service.received.load(Ordering::Relaxed),
        "MockService should have received the payload"
    );
    let received_id = service
        .received_id
        .lock()
        .clone()
        .expect("MockService should have recorded the bundle id");
    assert_eq!(received_id, bundle_id, "bundle id must round-trip the wire");
    assert_eq!(received_id.source, source);

    // Clean up
    drop(service.take_sink());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    server_tasks.shutdown().await;
}

// APP-CLI-05: Status notification (BPA pushes to application).
#[tokio::test]
async fn app_cli_05_status_notify() {
    let bpa = Arc::new(MockBpa::new());
    let (grpc_addr, server_tasks) = common::start_server(&bpa, &["application"]).await;

    let service = Arc::new(MockService::new());
    let remote_bpa = RemoteBpa::new(grpc_addr);

    let _endpoint: Eid = remote_bpa
        .register_application(hardy_bpv7::eid::Service::Ipn(42), service.clone())
        .await
        .expect("registration should succeed");

    assert!(!service.status_notified.load(Ordering::Relaxed));

    let server_service = bpa
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

    server_service
        .on_status_notify(
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
    drop(service.take_sink());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    server_tasks.shutdown().await;
}
