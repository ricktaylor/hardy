#![cfg(all(feature = "server", feature = "client"))]

//! Cross-crate lifecycle tests: the BpaClient SDK against a real BPA
//! behind the tonic transport, exercising the session endings the
//! in-crate suites do not reach — BPA-initiated teardown, connection
//! loss with a parked announcement, and simultaneous unregistration.

use std::{sync::Arc, time::Duration};

use hardy_async::TaskPool;
use hardy_bpa::{
    Bytes,
    bpa::Bpa,
    node_ids::NodeIds,
    services,
    stream::{Receiver, Segment},
};
use hardy_bpv7::eid::{Eid, IpnNodeId, NodeId, Service};
use hardy_proto::{
    application::application_service_server::ApplicationServiceServer,
    client::BpaClient,
    server::{ApplicationServiceImpl, Signer},
};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tonic::transport::{Server, server::TcpIncoming};

async fn timeout<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("test timed out")
}

struct Served {
    bpa: Arc<Bpa>,
    // Held live: dropping the pool would tear the sessions.
    tasks: TaskPool,
    url: String,
}

// A running BPA (node ipn:1) behind the application bridge on a
// port-0 listener.
async fn serve() -> Served {
    let node_ids = NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap();
    let bpa = Arc::new(Bpa::builder().node_ids(node_ids).build().await.unwrap());
    bpa.start(false);

    let tasks = TaskPool::new();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let incoming = TcpIncoming::from(listener).with_nodelay(Some(true));

    let service = ApplicationServiceServer::new(ApplicationServiceImpl::new(
        bpa.clone(),
        tasks.clone(),
        Signer::new(),
    ));
    tokio::spawn(
        Server::builder()
            .add_service(service)
            .serve_with_incoming(incoming),
    );

    Served {
        bpa,
        tasks,
        url: format!("http://{address}"),
    }
}

// The lifecycle events an application observes, in arrival order.
enum AppEvent {
    Registered,
    Unregistered,
    Delivered(Bytes),
}

struct LifecycleApp {
    sink: hardy_async::sync::spin::Once<Box<dyn services::ApplicationSink>>,
    events: mpsc::UnboundedSender<AppEvent>,
    // When unset, `on_register` drops the sink on the spot: the BPA
    // reads that as the application disconnecting.
    keep_sink: bool,
    // When set, `on_deliver` collects the payload and completes;
    // otherwise it declines (returns `Err`), so the bundle parks and is
    // re-announced to the next registration.
    collect: bool,
}

impl LifecycleApp {
    fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<AppEvent>) {
        Self::build(true, true)
    }

    fn dropping_its_sink() -> (Arc<Self>, mpsc::UnboundedReceiver<AppEvent>) {
        Self::build(false, true)
    }

    // Registers, but declines every delivery so the bundle parks.
    fn declining() -> (Arc<Self>, mpsc::UnboundedReceiver<AppEvent>) {
        Self::build(true, false)
    }

    fn build(keep_sink: bool, collect: bool) -> (Arc<Self>, mpsc::UnboundedReceiver<AppEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                events: tx,
                keep_sink,
                collect,
            }),
            rx,
        )
    }

    fn sink(&self) -> &dyn services::ApplicationSink {
        self.sink.get().unwrap().as_ref()
    }
}

#[hardy_bpa::async_trait]
impl services::Application for LifecycleApp {
    async fn on_register(&self, _source: &Eid, sink: Box<dyn services::ApplicationSink>) {
        if self.keep_sink {
            self.sink.call_once(|| sink);
        }
        let _ = self.events.send(AppEvent::Registered);
    }

    async fn on_unregister(&self) {
        let _ = self.events.send(AppEvent::Unregistered);
    }

    async fn on_deliver(
        &self,
        _bundle_id: &hardy_bpv7::bundle::Id,
        _expiry: time::OffsetDateTime,
        _ack_requested: bool,
        _adu_size: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        if self.collect {
            let data = hardy_bpa::stream::concat_stream(stream, usize::MAX).await?;
            let _ = self.events.send(AppEvent::Delivered(data));
            Ok(())
        } else {
            // Decline: the bundle parks and is re-announced to the next
            // registration on this endpoint.
            let _ = self.events.send(AppEvent::Delivered(Bytes::new()));
            Err(services::Error::StreamCancelled)
        }
    }

    async fn on_status_notify(
        &self,
        _bundle_id: &hardy_bpv7::bundle::Id,
        _from: &Eid,
        _kind: services::StatusNotify,
        _reason: hardy_bpv7::status_report::ReasonCode,
        _timestamp: Option<time::OffsetDateTime>,
    ) {
    }
}

async fn expect_registered(events: &mut mpsc::UnboundedReceiver<AppEvent>) {
    assert!(
        matches!(timeout(events.recv()).await, Some(AppEvent::Registered)),
        "expected the registration event"
    );
}

async fn expect_unregistered(events: &mut mpsc::UnboundedReceiver<AppEvent>) {
    loop {
        match timeout(events.recv()).await {
            Some(AppEvent::Unregistered) => return,
            // A late delivery may interleave; the stream is dead
            // anyway once the session is gone.
            Some(_) => continue,
            None => panic!("the application was never unregistered"),
        }
    }
}

// Registers `app` under `service_id`. A predecessor whose teardown the
// client observed (a round-tripped unregister) has already released the
// id, so this succeeds first try. After a peer's connection loss, though,
// a fresh client cannot observe the peer's server-side teardown, so the
// id can briefly read as in use: retry as fast as the round-trip
// completes (no sleep, no timing margin) until it frees. The deadline
// only bounds a regression.
async fn register_retrying(
    client: &BpaClient,
    app: &Arc<LifecycleApp>,
    service_id: u32,
) -> hardy_bpv7::eid::Eid {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match client
            .register_application(Service::Ipn(service_id), app.clone())
            .await
        {
            Ok(eid) => return eid,
            Err(services::Error::ServiceIdInUse(_)) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the predecessor session never released the service id"
                );
            }
            Err(e) => panic!("registration failed: {e}"),
        }
    }
}

/// A client-initiated unregister round-trips: the wire Unregister ends
/// the session, the SDK surfaces `on_unregister`, and the service id
/// frees for a successor registration.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_unregister_round_trips() {
    let served = serve().await;
    let client = BpaClient::new(served.url.clone(), TaskPool::new()).unwrap();

    let (app, mut events) = LifecycleApp::new();
    let eid = client
        .register_application(Service::Ipn(9), app.clone())
        .await
        .unwrap();
    assert_eq!(eid.to_string(), "ipn:1.9");
    expect_registered(&mut events).await;

    app.sink().unregister().await;
    expect_unregistered(&mut events).await;

    // The service id is free again.
    let (successor, mut successor_events) = LifecycleApp::new();
    register_retrying(&client, &successor, 9).await;
    expect_registered(&mut successor_events).await;

    served.bpa.shutdown().await;
}

/// BPA-initiated teardown reaches the client: shutting the BPA down
/// unregisters the bridge's component, which ends the wire session,
/// and the SDK surfaces `on_unregister` to the application.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bpa_initiated_teardown_reaches_the_client() {
    let served = serve().await;
    let client = BpaClient::new(served.url.clone(), TaskPool::new()).unwrap();

    let (app, mut events) = LifecycleApp::new();
    client
        .register_application(Service::Ipn(9), app.clone())
        .await
        .unwrap();
    expect_registered(&mut events).await;

    served.bpa.shutdown().await;
    expect_unregistered(&mut events).await;
}

/// Connection loss with an uncollected announcement defers, never
/// loses: the dead client's parked bundle is re-announced to the next
/// registration of the endpoint, which collects it whole.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_loss_defers_announced_bundles() {
    let served = serve().await;

    // The first client, on its own pool so its death can be driven.
    let doomed_tasks = TaskPool::new();
    let doomed_client = BpaClient::new(served.url.clone(), doomed_tasks.clone()).unwrap();
    let (doomed, mut doomed_events) = LifecycleApp::declining();
    let eid = doomed_client
        .register_application(Service::Ipn(9), doomed.clone())
        .await
        .unwrap();
    expect_registered(&mut doomed_events).await;

    // A bundle to self, announced but declined by the first client:
    // it parks for the next registration.
    let payload = Bytes::from_static(b"survives the connection");
    doomed
        .sink()
        .send(
            eid.clone(),
            Duration::from_secs(3600),
            None,
            &mut payload.clone(),
        )
        .await
        .unwrap();
    loop {
        match timeout(doomed_events.recv()).await {
            Some(AppEvent::Delivered(_)) => break,
            Some(_) => continue,
            None => panic!("the delivery was never announced"),
        }
    }

    // The first client declined and its connection dies: its pool tears
    // down, dropping the application drops its sink, whose request
    // sender half-closes the session stream, and the server unregisters
    // the session. The parked bundle awaits the next registration.
    doomed_tasks.shutdown().await;
    drop(doomed);
    drop(doomed_client);

    // A fresh client on the same endpoint is announced the parked
    // bundle afresh and collects it whole.
    let client = BpaClient::new(served.url, TaskPool::new()).unwrap();
    let (fresh, mut fresh_events) = LifecycleApp::new();
    register_retrying(&client, &fresh, 9).await;

    let collected = loop {
        match timeout(fresh_events.recv()).await {
            Some(AppEvent::Delivered(data)) => break data,
            Some(_) => continue,
            None => panic!("the parked bundle was never re-announced"),
        }
    };
    assert_eq!(collected, payload);

    served.bpa.shutdown().await;
}

/// Simultaneous unregistration from both ends settles: neither side
/// hangs, and the application observes exactly one unregistration.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simultaneous_unregister_settles() {
    let served = serve().await;
    let client = BpaClient::new(served.url.clone(), TaskPool::new()).unwrap();

    let (app, mut events) = LifecycleApp::new();
    client
        .register_application(Service::Ipn(9), app.clone())
        .await
        .unwrap();
    expect_registered(&mut events).await;

    // Both ends race their teardown.
    let bpa = served.bpa.clone();
    let client_side = {
        let app = app.clone();
        tokio::spawn(async move { app.sink().unregister().await })
    };
    let bpa_side = tokio::spawn(async move { bpa.shutdown().await });

    timeout(client_side).await.unwrap();
    timeout(bpa_side).await.unwrap();
    expect_unregistered(&mut events).await;

    // Exactly once: however many enders raced, the application must
    // observe a single unregistration.
    let extra = tokio::time::timeout(Duration::from_millis(500), events.recv()).await;
    assert!(
        !matches!(extra, Ok(Some(AppEvent::Unregistered))),
        "unregistration must be observed exactly once"
    );
}

/// An application that never stores its sink has disconnected by
/// definition: the dropped sink half-closes the session, the server
/// unregisters it, and the SDK surfaces `on_unregister`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_sink_unregisters() {
    let served = serve().await;
    let client = BpaClient::new(served.url.clone(), TaskPool::new()).unwrap();

    let (app, mut events) = LifecycleApp::dropping_its_sink();
    client
        .register_application(Service::Ipn(9), app.clone())
        .await
        .unwrap();
    expect_registered(&mut events).await;
    expect_unregistered(&mut events).await;

    // The service id frees for a successor.
    let (successor, mut successor_events) = LifecycleApp::new();
    register_retrying(&client, &successor, 9).await;
    expect_registered(&mut successor_events).await;

    served.bpa.shutdown().await;
}

/// The server losing its sessions (a bridge teardown: a restart from
/// the client's point of view) is a disconnection, not a hang: the SDK
/// surfaces `on_unregister`, and the orphaned sink fails rather than
/// blocking, its token dead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_restart_disconnects_the_client() {
    let served = serve().await;
    let client = BpaClient::new(served.url.clone(), TaskPool::new()).unwrap();

    let (app, mut events) = LifecycleApp::new();
    let eid = client
        .register_application(Service::Ipn(9), app.clone())
        .await
        .unwrap();
    expect_registered(&mut events).await;

    served.tasks.shutdown().await;
    expect_unregistered(&mut events).await;

    let result = app
        .sink()
        .send(
            eid,
            Duration::from_secs(3600),
            None,
            &mut Bytes::from_static(b"into the void"),
        )
        .await;
    assert!(result.is_err(), "a dead session must fail the send");

    served.bpa.shutdown().await;
}
