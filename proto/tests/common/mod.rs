// Each test binary uses a different subset of these fixtures, so unused helpers are expected.
#![allow(dead_code)]

use core::{future::Future, num::NonZeroU64};
use std::{borrow::Cow, net::SocketAddr, sync::Arc, time::Duration};

use hardy_bpa::{
    Bytes, async_trait,
    bpa::{Bpa, BpaRegistration},
    cla::{self, Cla, ClaInit, ForwardBundleResult},
    node_ids::NodeIds,
    policy::FlowControllerFactory,
    routing::{self, RoutingAgent, RoutingSink},
    services::{
        self, Application, ApplicationSink, Service as BpaService, ServiceSink, StatusNotify,
    },
    stream::{Receiver, Segment},
};
use hardy_bpv7::{
    builder::Builder,
    bundle::Id as BundleId,
    creation_timestamp::CreationTimestamp,
    eid::{Eid, IpnNodeId, NodeId, Service},
    status_report::ReasonCode,
};
use time::OffsetDateTime;
use tokio::{net::TcpListener, sync::broadcast};
use tonic::transport::server::{Router, TcpIncoming};

// The timeout only bounds a regression; correct code completes at once.
pub async fn timeout<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("test timed out")
}

pub fn ipn1() -> NodeIds {
    NodeIds::try_from(
        [NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 1,
        })]
        .as_slice(),
    )
    .unwrap()
}

pub fn build_bundle(source: &str, destination: &str, payload: &[u8]) -> Bytes {
    let (_, data) = Builder::new(source.parse().unwrap(), destination.parse().unwrap())
        .with_payload(Cow::Borrowed(payload))
        .build(CreationTimestamp::now())
        .unwrap();
    Bytes::from(data)
}

pub async fn build_bpa(node_ids: NodeIds, status_reports: bool) -> Arc<Bpa> {
    let bpa = Arc::new(
        Bpa::builder()
            .node_ids(node_ids)
            .status_reports(status_reports)
            .build()
            .await
            .unwrap(),
    );
    bpa.start(false).await;
    bpa
}

pub async fn serve(router: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let incoming = TcpIncoming::from(listener).with_nodelay(Some(true));
    tokio::spawn(router.serve_with_incoming(incoming));
    address
}

pub async fn wait_unregistered(unregistered: &mut broadcast::Receiver<()>) {
    timeout(unregistered.recv()).await.unwrap();
}

pub struct UnregisterWatch {
    bpa: Arc<dyn BpaRegistration>,
    unregistered: broadcast::Sender<()>,
}

impl UnregisterWatch {
    pub fn new(bpa: Arc<dyn BpaRegistration>) -> Arc<Self> {
        Arc::new(Self {
            bpa,
            unregistered: broadcast::channel(16).0,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.unregistered.subscribe()
    }
}

#[async_trait]
impl BpaRegistration for UnregisterWatch {
    async fn register_cla(
        &self,
        name: String,
        cla: Arc<dyn Cla>,
        policy: Option<Arc<dyn FlowControllerFactory>>,
        init: ClaInit,
    ) -> cla::Result<Vec<NodeId>> {
        self.bpa
            .register_cla(
                name,
                Arc::new(WatchedCla {
                    inner: cla,
                    unregistered: self.unregistered.clone(),
                }),
                policy,
                init,
            )
            .await
    }

    async fn register_service(
        &self,
        service_id: Service,
        service: Arc<dyn BpaService>,
    ) -> services::Result<Eid> {
        self.bpa
            .register_service(
                service_id,
                Arc::new(WatchedService {
                    inner: service,
                    unregistered: self.unregistered.clone(),
                }),
            )
            .await
    }

    async fn register_application(
        &self,
        service_id: Service,
        application: Arc<dyn Application>,
    ) -> services::Result<Eid> {
        self.bpa
            .register_application(
                service_id,
                Arc::new(WatchedApplication {
                    inner: application,
                    unregistered: self.unregistered.clone(),
                }),
            )
            .await
    }

    async fn register_dynamic_service(
        &self,
        service: Arc<dyn BpaService>,
    ) -> services::Result<Eid> {
        self.bpa
            .register_dynamic_service(Arc::new(WatchedService {
                inner: service,
                unregistered: self.unregistered.clone(),
            }))
            .await
    }

    async fn register_dynamic_application(
        &self,
        application: Arc<dyn Application>,
    ) -> services::Result<Eid> {
        self.bpa
            .register_dynamic_application(Arc::new(WatchedApplication {
                inner: application,
                unregistered: self.unregistered.clone(),
            }))
            .await
    }

    async fn register_routing_agent(
        &self,
        name: String,
        agent: Arc<dyn RoutingAgent>,
    ) -> routing::Result<Vec<NodeId>> {
        self.bpa
            .register_routing_agent(
                name,
                Arc::new(WatchedAgent {
                    inner: agent,
                    unregistered: self.unregistered.clone(),
                }),
            )
            .await
    }
}

struct WatchedApplication {
    inner: Arc<dyn Application>,
    unregistered: broadcast::Sender<()>,
}

#[async_trait]
impl Application for WatchedApplication {
    async fn on_register(&self, source: &Eid, sink: Box<dyn ApplicationSink>) {
        self.inner.on_register(source, sink).await
    }

    async fn on_unregister(&self) {
        self.inner.on_unregister().await;
        let _ = self.unregistered.send(());
    }

    async fn on_deliver(
        &self,
        bundle_id: &BundleId,
        expiry: OffsetDateTime,
        ack_requested: bool,
        adu_size: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        self.inner
            .on_deliver(bundle_id, expiry, ack_requested, adu_size, stream)
            .await
    }

    async fn on_status_notify(
        &self,
        bundle_id: &BundleId,
        from: &Eid,
        kind: StatusNotify,
        reason: ReasonCode,
        timestamp: Option<OffsetDateTime>,
    ) {
        self.inner
            .on_status_notify(bundle_id, from, kind, reason, timestamp)
            .await
    }
}

struct WatchedService {
    inner: Arc<dyn BpaService>,
    unregistered: broadcast::Sender<()>,
}

#[async_trait]
impl BpaService for WatchedService {
    async fn on_register(&self, endpoint: &Eid, sink: Box<dyn ServiceSink>) {
        self.inner.on_register(endpoint, sink).await
    }

    async fn on_unregister(&self) {
        self.inner.on_unregister().await;
        let _ = self.unregistered.send(());
    }

    async fn on_deliver(
        &self,
        bundle_id: &BundleId,
        expiry: OffsetDateTime,
        bundle_size: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        self.inner
            .on_deliver(bundle_id, expiry, bundle_size, stream)
            .await
    }

    async fn on_status_notify(
        &self,
        bundle_id: &BundleId,
        from: &Eid,
        kind: StatusNotify,
        reason: ReasonCode,
        timestamp: Option<OffsetDateTime>,
    ) {
        self.inner
            .on_status_notify(bundle_id, from, kind, reason, timestamp)
            .await
    }
}

struct WatchedCla {
    inner: Arc<dyn Cla>,
    unregistered: broadcast::Sender<()>,
}

#[async_trait]
impl Cla for WatchedCla {
    async fn on_register(
        &self,
        sink: Box<dyn cla::Sink>,
        node_ids: &[NodeId],
        max_bundle_size: Option<NonZeroU64>,
    ) {
        self.inner
            .on_register(sink, node_ids, max_bundle_size)
            .await
    }

    async fn on_unregister(&self) {
        self.inner.on_unregister().await;
        let _ = self.unregistered.send(());
    }

    async fn forward(
        &self,
        lane: Option<u32>,
        cla_addr: &cla::ClaAddress,
        bundle_id: &BundleId,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> cla::Result<ForwardBundleResult> {
        self.inner
            .forward(lane, cla_addr, bundle_id, total_len, stream)
            .await
    }
}

struct WatchedAgent {
    inner: Arc<dyn RoutingAgent>,
    unregistered: broadcast::Sender<()>,
}

#[async_trait]
impl RoutingAgent for WatchedAgent {
    async fn on_register(&self, sink: Box<dyn RoutingSink>, node_ids: &[NodeId]) {
        self.inner.on_register(sink, node_ids).await
    }

    async fn on_unregister(&self) {
        self.inner.on_unregister().await;
        let _ = self.unregistered.send(());
    }
}
