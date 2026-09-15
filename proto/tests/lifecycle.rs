#![cfg(all(feature = "server", feature = "client"))]

mod common;

use std::{
    error::Error as _, future::pending, io, net::SocketAddr, pin::pin, sync::Arc, time::Duration,
};

use hardy_async::{TaskPool, sync::spin::Once};
use hardy_bpa::{
    Bytes, async_trait,
    bpa::Bpa,
    services,
    stream::{Receiver, RecvError, Segment, concat_stream},
};
use hardy_bpv7::{
    bundle::Id as BundleId,
    eid::{Eid, Service},
    status_report::ReasonCode,
};
use hardy_proto::{
    application::application_service_server::ApplicationServiceServer, client::BpaClient,
    server::ApplicationServiceImpl,
};
use time::OffsetDateTime;
use tokio::{
    io::copy_bidirectional,
    net::{TcpListener, TcpStream},
    spawn,
    sync::{
        Barrier,
        mpsc::{self, error::TryRecvError},
    },
    task::{JoinHandle, JoinSet},
};
use tonic::{Code, Status, transport::Server};

use common::{UnregisterWatch, build_bpa, ipn1, serve, timeout, wait_unregistered};

struct Harness {
    bpa: Arc<Bpa>,
    watch: Arc<UnregisterWatch>,
    tasks: TaskPool,
    address: SocketAddr,
    url: String,
}

async fn harness() -> Harness {
    let bpa = build_bpa(ipn1(), false).await;
    let tasks = TaskPool::new();
    let watch = UnregisterWatch::new(bpa.clone());
    let service =
        ApplicationServiceServer::new(ApplicationServiceImpl::new(watch.clone(), tasks.clone()));
    let address = serve(Server::builder().add_service(service)).await;

    Harness {
        bpa,
        watch,
        tasks,
        address,
        url: format!("http://{address}"),
    }
}

// The proxy runs until aborted; a failure before that is returned rather than
// unwrapped, so `assert_ran_until_aborted` can name it.
async fn killable_proxy(upstream: SocketAddr) -> (SocketAddr, JoinHandle<io::Result<()>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let proxy = spawn(async move {
        let mut connections = JoinSet::new();
        loop {
            let (mut inbound, _) = listener.accept().await?;
            let mut outbound = TcpStream::connect(upstream).await?;
            connections.spawn(async move {
                let _ = copy_bidirectional(&mut inbound, &mut outbound).await;
            });
        }
    });
    (address, proxy)
}

async fn assert_ran_until_aborted(proxy: JoinHandle<io::Result<()>>) {
    match timeout(proxy).await {
        Err(join) if join.is_cancelled() => {}
        other => panic!("the proxy must run until it is aborted, got {other:?}"),
    }
}

enum AppEvent {
    Registered,
    Unregistered,
    Delivered(Bytes),
}

enum DeliveryMode {
    Collect,
    Decline,
    Stall,
    Rendezvous(Arc<Barrier>),
}

struct LifecycleApp {
    sink: Once<Box<dyn services::ApplicationSink>>,
    events: mpsc::UnboundedSender<AppEvent>,
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

    fn declining() -> (Arc<Self>, mpsc::UnboundedReceiver<AppEvent>) {
        Self::build(true, DeliveryMode::Decline)
    }

    fn stalling() -> (Arc<Self>, mpsc::UnboundedReceiver<AppEvent>) {
        Self::build(true, DeliveryMode::Stall)
    }

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
                sink: Once::new(),
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

#[async_trait]
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
        _bundle_id: &BundleId,
        _expiry: OffsetDateTime,
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
                let _ = self.events.send(AppEvent::Delivered(Bytes::new()));
                Err(services::Error::Internal("test: declined".into()))
            }
            DeliveryMode::Stall => {
                let _ = self.events.send(AppEvent::Delivered(Bytes::new()));
                // Reads past the last chunk, where the server holds the
                // stream open for the ack, so the delivery stays blocked on
                // the stream until the session ends it.
                loop {
                    stream
                        .recv()
                        .await
                        .map_err(|_| services::Error::StreamCancelled)?;
                }
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
        _bundle_id: &BundleId,
        _from: &Eid,
        _kind: services::StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<OffsetDateTime>,
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
            Some(_) => continue,
            None => panic!("the application was never unregistered"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_unregister_round_trips() {
    let served = harness().await;
    let mut unregistered = served.watch.subscribe();
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

    timeout(handle.join())
        .await
        .expect("a round-tripped unregister must end the session cleanly");

    wait_unregistered(&mut unregistered).await;
    let (successor, mut successor_events) = LifecycleApp::new();
    let _successor = client
        .register_application(Service::Ipn(9), successor.clone())
        .await
        .unwrap();
    expect_registered(&mut successor_events).await;

    served.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bpa_initiated_teardown_reaches_the_client() {
    let served = harness().await;
    let client = BpaClient::new(served.url.clone(), TaskPool::new()).unwrap();

    let (app, mut events) = LifecycleApp::new();
    let handle = client
        .register_application(Service::Ipn(9), app.clone())
        .await
        .unwrap();
    expect_registered(&mut events).await;

    served.bpa.shutdown().await;
    expect_unregistered(&mut events).await;

    assert!(
        matches!(
            timeout(handle.join()).await,
            Err(services::Error::Disconnected)
        ),
        "an unsolicited close must reach the caller as a disconnection"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_pool_shutdown_defers_a_declined_delivery() {
    let served = harness().await;
    let mut unregistered = served.watch.subscribe();

    let doomed_tasks = TaskPool::new();
    let doomed_client = BpaClient::new(served.url.clone(), doomed_tasks.clone()).unwrap();
    let (doomed, mut doomed_events) = LifecycleApp::declining();
    let handle = doomed_client
        .register_application(Service::Ipn(9), doomed.clone())
        .await
        .unwrap();
    let eid = handle.id().clone();
    expect_registered(&mut doomed_events).await;

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

    doomed_tasks.shutdown().await;
    drop(doomed);
    drop(doomed_client);

    wait_unregistered(&mut unregistered).await;
    let client = BpaClient::new(served.url, TaskPool::new()).unwrap();
    let (fresh, mut fresh_events) = LifecycleApp::new();
    let _fresh = client
        .register_application(Service::Ipn(9), fresh.clone())
        .await
        .unwrap();

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simultaneous_unregister_settles() {
    let served = harness().await;
    let tasks = TaskPool::new();
    let client = BpaClient::new(served.url.clone(), tasks.clone()).unwrap();

    let (app, mut events) = LifecycleApp::new();
    let _handle = client
        .register_application(Service::Ipn(9), app.clone())
        .await
        .unwrap();
    expect_registered(&mut events).await;

    let bpa = served.bpa.clone();
    let client_side = {
        let app = app.clone();
        spawn(async move { app.sink().unregister().await })
    };
    let bpa_side = spawn(async move { bpa.shutdown().await });

    timeout(client_side).await.unwrap();
    timeout(bpa_side).await.unwrap();
    expect_unregistered(&mut events).await;

    timeout(tasks.shutdown()).await;

    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(event, AppEvent::Unregistered),
            "unregistration must be observed exactly once"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_sink_unregisters() {
    let served = harness().await;
    let mut unregistered = served.watch.subscribe();
    let client = BpaClient::new(served.url.clone(), TaskPool::new()).unwrap();

    let (app, mut events) = LifecycleApp::dropping_its_sink();
    let handle = client
        .register_application(Service::Ipn(9), app.clone())
        .await
        .unwrap();
    expect_registered(&mut events).await;
    expect_unregistered(&mut events).await;

    timeout(handle.join())
        .await
        .expect("a dropped sink must end the session cleanly");

    wait_unregistered(&mut unregistered).await;
    let (successor, mut successor_events) = LifecycleApp::new();
    let _successor = client
        .register_application(Service::Ipn(9), successor.clone())
        .await
        .unwrap();
    expect_registered(&mut successor_events).await;

    served.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_registration_on_a_shut_down_client_is_refused() {
    let served = harness().await;
    let tasks = TaskPool::new();
    let client = BpaClient::new(served.url.clone(), tasks.clone()).unwrap();
    tasks.shutdown().await;

    let (app, mut events) = LifecycleApp::new();
    let result = timeout(client.register_application(Service::Ipn(9), app.clone())).await;

    assert!(
        matches!(result, Err(services::Error::Disconnected)),
        "a shut down client must refuse a registration"
    );
    // The refusal came from the client's own shut-down pool, with nothing
    // left running, so the channel is quiet rather than closed.
    assert!(
        matches!(events.try_recv(), Err(TryRecvError::Empty)),
        "a refused registration must not have registered the application"
    );

    served.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_shutdown_releases_the_registration() {
    let served = harness().await;
    let mut unregistered = served.watch.subscribe();
    let tasks = TaskPool::new();
    let client = BpaClient::new(served.url.clone(), tasks.clone()).unwrap();

    let (app, mut events) = LifecycleApp::new();
    let handle = client
        .register_application(Service::Ipn(9), app.clone())
        .await
        .unwrap();
    expect_registered(&mut events).await;

    tasks.shutdown().await;
    expect_unregistered(&mut events).await;
    timeout(handle.join())
        .await
        .expect("a pool shutdown must end the session cleanly");

    wait_unregistered(&mut unregistered).await;
    let successor_client = BpaClient::new(served.url.clone(), TaskPool::new()).unwrap();
    let (successor, mut successor_events) = LifecycleApp::new();
    let _successor = successor_client
        .register_application(Service::Ipn(9), successor.clone())
        .await
        .unwrap();
    expect_registered(&mut successor_events).await;

    served.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_pool_shutdown_disconnects_the_client() {
    let served = harness().await;
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

    assert!(
        matches!(
            timeout(handle.join()).await,
            Err(services::Error::Disconnected)
        ),
        "a bridge teardown must read as a disconnection"
    );

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transport_loss_surfaces_the_session_error() {
    let served = harness().await;
    let (proxy_address, proxy) = killable_proxy(served.address).await;
    let client = BpaClient::new(format!("http://{proxy_address}"), TaskPool::new()).unwrap();

    let (app, mut events) = LifecycleApp::new();
    let handle = client
        .register_application(Service::Ipn(9), app.clone())
        .await
        .unwrap();
    expect_registered(&mut events).await;

    proxy.abort();
    assert_ran_until_aborted(proxy).await;

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_ends_the_stream_a_delivery_is_blocked_on() {
    let served = harness().await;
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

    loop {
        match timeout(events.recv()).await {
            Some(AppEvent::Delivered(_)) => break,
            Some(_) => continue,
            None => panic!("the delivery was never announced"),
        }
    }

    timeout(tasks.shutdown()).await;
    expect_unregistered(&mut events).await;

    served.bpa.shutdown().await;
}

#[ignore = "the BPA serialises deliveries per service, so a held-open collection blocks the next announcement: see docs/TODO.md"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deliveries_collect_concurrently() {
    let served = harness().await;
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

struct StalledProducer;

#[async_trait]
impl Receiver<Segment> for StalledProducer {
    async fn recv(&mut self) -> Result<Segment, RecvError> {
        pending().await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dead_session_fails_a_send_from_a_stalled_producer() {
    let served = harness().await;
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

    let result = timeout(app.sink().send(
        eid,
        Duration::from_secs(3600),
        None,
        None,
        &mut StalledProducer,
    ))
    .await;
    assert!(
        matches!(result, Err(services::Error::Disconnected)),
        "a dead session must fail the send as disconnected"
    );

    served.bpa.shutdown().await;
}

// An application that holds the registration open inside `on_register`, so a
// test can give up on a registration while the component is being registered.
struct SlowApp {
    started: Arc<Barrier>,
    release: Arc<Barrier>,
    events: mpsc::UnboundedSender<AppEvent>,
}

impl SlowApp {
    fn new() -> (
        Arc<Self>,
        Arc<Barrier>,
        Arc<Barrier>,
        mpsc::UnboundedReceiver<AppEvent>,
    ) {
        let started = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                started: started.clone(),
                release: release.clone(),
                events: tx,
            }),
            started,
            release,
            rx,
        )
    }
}

#[async_trait]
impl services::Application for SlowApp {
    async fn on_register(&self, _source: &Eid, _sink: Box<dyn services::ApplicationSink>) {
        self.started.wait().await;
        self.release.wait().await;
        let _ = self.events.send(AppEvent::Registered);
    }

    async fn on_unregister(&self) {
        let _ = self.events.send(AppEvent::Unregistered);
    }

    async fn on_deliver(
        &self,
        _bundle_id: &BundleId,
        _expiry: OffsetDateTime,
        _ack_requested: bool,
        _adu_size: u64,
        _stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        unreachable!("the test never delivers");
    }

    async fn on_status_notify(
        &self,
        _bundle_id: &BundleId,
        _from: &Eid,
        _kind: services::StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<OffsetDateTime>,
    ) {
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_registration_the_caller_gives_up_on_is_unregistered() {
    let served = harness().await;
    let mut unregistered = served.watch.subscribe();
    let client = BpaClient::new(served.url.clone(), TaskPool::new()).unwrap();

    let (app, started, release, mut events) = SlowApp::new();
    {
        let mut registering = pin!(client.register_application(Service::Ipn(9), app.clone()));
        tokio::select! {
            _ = &mut registering => {
                panic!("a registration cannot finish while `on_register` is still running")
            }
            _ = started.wait() => {}
        }
    }
    release.wait().await;

    expect_unregistered(&mut events).await;
    wait_unregistered(&mut unregistered).await;

    // The service id the abandoned registration held is free again.
    let (successor, mut successor_events) = LifecycleApp::new();
    let _successor = timeout(client.register_application(Service::Ipn(9), successor.clone()))
        .await
        .expect("an abandoned registration must leave its service id free");
    expect_registered(&mut successor_events).await;

    served.bpa.shutdown().await;
}
