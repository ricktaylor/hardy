#![cfg(all(feature = "server", feature = "client"))]

//! Cross-crate lifecycle tests: the BpaClient SDK against a real BPA
//! behind the tonic transport, exercising the session endings the
//! in-crate suites do not reach — BPA-initiated teardown, connection
//! loss with a parked announcement, and simultaneous unregistration.

use std::{error::Error as _, future::pending, net::SocketAddr, sync::Arc, time::Duration};

use hardy_async::TaskPool;
use hardy_bpa::{
    Bytes,
    bpa::Bpa,
    node_ids::NodeIds,
    services,
    stream::{Receiver, Segment, concat_stream},
};
use hardy_bpv7::eid::{Eid, IpnNodeId, NodeId, Service};
use hardy_proto::{
    application::application_service_server::ApplicationServiceServer,
    client::{BpaClient, RegistrationHandle},
    server::ApplicationServiceImpl,
};
use tokio::{
    io::copy_bidirectional,
    net::{TcpListener, TcpStream},
    sync::{Barrier, mpsc},
    task::{JoinHandle, JoinSet},
};
use tonic::{
    Code, Status,
    transport::{Server, server::TcpIncoming},
};

async fn timeout<F: Future>(future: F) -> F::Output {
    // The timeout only bounds a regression: the wait it wraps is event-driven.
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("test timed out")
}

struct Served {
    bpa: Arc<Bpa>,
    // Held live: dropping the pool would tear the sessions.
    tasks: TaskPool,
    address: SocketAddr,
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

    let service =
        ApplicationServiceServer::new(ApplicationServiceImpl::new(bpa.clone(), tasks.clone()));
    tokio::spawn(
        Server::builder()
            .add_service(service)
            .serve_with_incoming(incoming),
    );

    Served {
        bpa,
        tasks,
        address,
        url: format!("http://{address}"),
    }
}

// A byte-level TCP proxy in front of `upstream`, so a test can sever
// live connections without trailers: aborting the returned task drops
// its `JoinSet`, which aborts every per-connection pump and closes both
// sockets mid-stream. Returns the address to dial and the task to abort.
async fn killable_proxy(upstream: SocketAddr) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let proxy = tokio::spawn(async move {
        let mut connections = JoinSet::new();
        loop {
            let (mut inbound, _) = listener.accept().await.unwrap();
            let mut outbound = TcpStream::connect(upstream).await.unwrap();
            connections.spawn(async move {
                let _ = copy_bidirectional(&mut inbound, &mut outbound).await;
            });
        }
    });
    (address, proxy)
}

// The lifecycle events an application observes, in arrival order.
enum AppEvent {
    Registered,
    Unregistered,
    Delivered(Bytes),
}

// What `on_deliver` does with an announced delivery.
enum DeliveryMode {
    // Collects the payload whole and completes.
    Collect,
    // Announces the delivery, then declines it (returns `Err` without
    // pulling the stream), so the bundle parks and is re-announced to
    // the next registration.
    Decline,
    // Announces the delivery, then parks forever: a stuck collection,
    // standing in for a wedged application or server.
    Stall,
    // Waits at the shared barrier before collecting, so the test can
    // require several deliveries to be inside `on_deliver` at once.
    Rendezvous(Arc<Barrier>),
}

struct LifecycleApp {
    sink: hardy_async::sync::spin::Once<Box<dyn services::ApplicationSink>>,
    events: mpsc::UnboundedSender<AppEvent>,
    // When unset, `on_register` drops the sink on the spot: the BPA
    // reads that as the application disconnecting.
    keep_sink: bool,
    mode: DeliveryMode,
}

impl LifecycleApp {
    fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<AppEvent>) {
        Self::build(true, DeliveryMode::Collect)
    }

    fn dropping_its_sink() -> (Arc<Self>, mpsc::UnboundedReceiver<AppEvent>) {
        Self::build(false, DeliveryMode::Collect)
    }

    // Registers, but declines every delivery so the bundle parks.
    fn declining() -> (Arc<Self>, mpsc::UnboundedReceiver<AppEvent>) {
        Self::build(true, DeliveryMode::Decline)
    }

    // Registers, but every delivery sticks inside `on_deliver` forever.
    fn stalling() -> (Arc<Self>, mpsc::UnboundedReceiver<AppEvent>) {
        Self::build(true, DeliveryMode::Stall)
    }

    // Registers; every delivery must reach `on_deliver` together with
    // `parties - 1` others before any of them collects.
    fn rendezvousing(parties: usize) -> (Arc<Self>, mpsc::UnboundedReceiver<AppEvent>) {
        Self::build(
            true,
            DeliveryMode::Rendezvous(Arc::new(Barrier::new(parties))),
        )
    }

    fn build(
        keep_sink: bool,
        mode: DeliveryMode,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<AppEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                events: tx,
                keep_sink,
                mode,
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
        match &self.mode {
            DeliveryMode::Collect => {
                let data = concat_stream(stream, usize::MAX, None).await?;
                let _ = self.events.send(AppEvent::Delivered(data));
                Ok(())
            }
            DeliveryMode::Decline => {
                // Decline with a clearly synthetic error, distinct from
                // the SDK's own transfer-cancel error: any `Err` before
                // the stream is pulled to completion parks the bundle
                // for re-announcement to the next registration.
                let _ = self.events.send(AppEvent::Delivered(Bytes::new()));
                Err(services::Error::Internal("test: declined".into()))
            }
            DeliveryMode::Stall => {
                let _ = self.events.send(AppEvent::Delivered(Bytes::new()));
                pending().await
            }
            DeliveryMode::Rendezvous(barrier) => {
                barrier.wait().await;
                let data = concat_stream(stream, usize::MAX, None).await?;
                let _ = self.events.send(AppEvent::Delivered(data));
                Ok(())
            }
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

// Registers `app` under `service_id`, retrying until it takes, and returns
// the registration handle. A predecessor whose teardown the client
// observed (a round-tripped unregister) has already released the id, so
// this succeeds first try. After a peer's connection loss, though, a
// fresh client cannot observe the peer's server-side teardown, so the id
// can briefly read as in use: `register_application` returns
// `ServiceIdInUse`, and this retries as fast as the round-trip completes
// (no sleep, no timing margin) until it frees. The deadline only bounds a
// regression.
async fn register_retrying(
    client: &BpaClient,
    app: &Arc<LifecycleApp>,
    service_id: u32,
) -> RegistrationHandle<Eid, services::Error> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match client
            .register_application(Service::Ipn(service_id), app.clone())
            .await
        {
            Ok(handle) => return handle,
            Err(services::Error::ServiceIdInUse(_)) => assert!(
                tokio::time::Instant::now() < deadline,
                "the predecessor session never released the service id"
            ),
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
    let handle = client
        .register_application(Service::Ipn(9), app.clone())
        .await
        .unwrap();
    let eid = handle.id().clone();
    expect_registered(&mut events).await;
    assert_eq!(eid.to_string(), "ipn:1.9");

    app.sink().unregister().await;
    expect_unregistered(&mut events).await;

    // The handle observes the round-tripped close as a clean end.
    timeout(handle.join())
        .await
        .expect("a round-tripped unregister must end the session cleanly");

    // The service id is free again.
    let (successor, mut successor_events) = LifecycleApp::new();
    let _successor = register_retrying(&client, &successor, 9).await;
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
    let _handle = client
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
    let handle = doomed_client
        .register_application(Service::Ipn(9), doomed.clone())
        .await
        .unwrap();
    let eid = handle.id().clone();
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
    let _fresh = register_retrying(&client, &fresh, 9).await;

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
    let tasks = TaskPool::new();
    let client = BpaClient::new(served.url.clone(), tasks.clone()).unwrap();

    let (app, mut events) = LifecycleApp::new();
    let _handle = client
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

    // Joining the client's pool joins the session task, the only sender
    // of lifecycle events: afterwards the channel already holds every
    // event the application will ever observe, so absence is proved by
    // draining it, not by a quiet window.
    timeout(tasks.shutdown()).await;

    // Exactly once: however many enders raced, the application must
    // observe a single unregistration.
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(event, AppEvent::Unregistered),
            "unregistration must be observed exactly once"
        );
    }
}

/// An application that never stores its sink has disconnected by
/// definition: the dropped sink half-closes the session, the server
/// unregisters it, and the SDK surfaces `on_unregister`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_sink_unregisters() {
    let served = serve().await;
    let client = BpaClient::new(served.url.clone(), TaskPool::new()).unwrap();

    let (app, mut events) = LifecycleApp::dropping_its_sink();
    let _handle = client
        .register_application(Service::Ipn(9), app.clone())
        .await
        .unwrap();
    expect_registered(&mut events).await;
    expect_unregistered(&mut events).await;

    // The service id frees for a successor.
    let (successor, mut successor_events) = LifecycleApp::new();
    let _successor = register_retrying(&client, &successor, 9).await;
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
    let handle = client
        .register_application(Service::Ipn(9), app.clone())
        .await
        .unwrap();
    let eid = handle.id().clone();
    expect_registered(&mut events).await;

    served.tasks.shutdown().await;
    expect_unregistered(&mut events).await;

    // The bridge tears its sessions down with clean trailers, so the
    // handle reads a bridge loss as the BPA closing the session, not a
    // stream failure; the orphaned sink below carries the error.
    timeout(handle.join())
        .await
        .expect("a bridge teardown must read as a clean close");

    let result = app
        .sink()
        .send(
            eid,
            Duration::from_secs(3600),
            None,
            None,
            &mut Bytes::from_static(b"into the void"),
        )
        .await;
    assert!(
        matches!(result, Err(services::Error::Disconnected)),
        "a dead session must fail the send as disconnected"
    );

    served.bpa.shutdown().await;
}

/// A hard transport loss surfaces the actual failure through the
/// registration handle: a connection killed without trailers ends the
/// session with the transport's own error carried whole (nothing
/// flattened to a category), unlike the clean `Ok` of an orderly close.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transport_loss_surfaces_the_session_error() {
    let served = serve().await;
    // The client dials through a killable proxy, because only severing
    // the TCP connection itself ends the stream without trailers; every
    // in-process teardown closes it cleanly.
    let (proxy_address, proxy) = killable_proxy(served.address).await;
    let client = BpaClient::new(format!("http://{proxy_address}"), TaskPool::new()).unwrap();

    let (app, mut events) = LifecycleApp::new();
    let handle = client
        .register_application(Service::Ipn(9), app.clone())
        .await
        .unwrap();
    expect_registered(&mut events).await;

    // Kill the transport underneath the live session.
    proxy.abort();

    // The ending bubbles as the transport's own status: tonic reports
    // the severed connection as `Unknown` carrying the I/O failure as
    // its source, and both survive to the handle.
    let Err(services::Error::Internal(e)) = timeout(handle.join()).await else {
        panic!("a killed transport must end the session with its own error");
    };
    let status = e
        .downcast::<Status>()
        .expect("the session error must be the transport's own status");
    assert_eq!(status.code(), Code::Unknown);
    assert!(
        status.source().is_some(),
        "the transport failure's source chain must survive to the handle"
    );
    expect_unregistered(&mut events).await;

    served.bpa.shutdown().await;
}

/// A stuck collection cannot hang shutdown: an `on_deliver` that never
/// returns is abandoned when the client's pool shuts down, and the
/// session still runs its unregistration to completion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_interrupts_a_stuck_delivery() {
    let served = serve().await;
    let tasks = TaskPool::new();
    let client = BpaClient::new(served.url.clone(), tasks.clone()).unwrap();

    let (app, mut events) = LifecycleApp::stalling();
    let handle = client
        .register_application(Service::Ipn(9), app.clone())
        .await
        .unwrap();
    let eid = handle.id().clone();
    expect_registered(&mut events).await;

    app.sink()
        .send(
            eid,
            Duration::from_secs(3600),
            None,
            None,
            &mut Bytes::from_static(b"never collected").clone(),
        )
        .await
        .unwrap();

    // The application is now inside `on_deliver`, parked forever.
    loop {
        match timeout(events.recv()).await {
            Some(AppEvent::Delivered(_)) => break,
            Some(_) => continue,
            None => panic!("the delivery was never announced"),
        }
    }

    // Shutting the pool down must abandon the stuck delivery and join
    // the session; the timeout only bounds a regression.
    timeout(tasks.shutdown()).await;
    expect_unregistered(&mut events).await;

    served.bpa.shutdown().await;
}

/// Deliveries collect concurrently: two announced bundles are inside
/// `on_deliver` at the same time, so one slow collection does not stall
/// the next announcement. With serial delivery the first `on_deliver`
/// would hold the loop and the rendezvous barrier would never release;
/// the timeout only bounds a regression.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deliveries_collect_concurrently() {
    let served = serve().await;
    let client = BpaClient::new(served.url.clone(), TaskPool::new()).unwrap();

    let (app, mut events) = LifecycleApp::rendezvousing(2);
    let handle = client
        .register_application(Service::Ipn(9), app.clone())
        .await
        .unwrap();
    let eid = handle.id().clone();
    expect_registered(&mut events).await;

    let first = Bytes::from_static(b"first of the pair");
    let second = Bytes::from_static(b"second of the pair");
    for payload in [&first, &second] {
        app.sink()
            .send(
                eid.clone(),
                Duration::from_secs(3600),
                None,
                None,
                &mut payload.clone(),
            )
            .await
            .unwrap();
    }

    let mut collected = Vec::new();
    while collected.len() < 2 {
        match timeout(events.recv()).await {
            Some(AppEvent::Delivered(data)) => collected.push(data),
            Some(_) => continue,
            None => panic!("both deliveries must complete"),
        }
    }
    collected.sort();
    let mut expected = vec![first, second];
    expected.sort();
    assert_eq!(collected, expected);

    served.bpa.shutdown().await;
}
