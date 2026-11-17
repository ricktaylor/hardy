//! `BpaBuilder`'s configured registrations: CLAs, services and
//! applications are converted when `build()` runs, a failure there is a
//! typed `builder::Error` naming the component, and the converted
//! components go live at `start()`.

use core::{
    num::NonZeroU64,
    sync::atomic::{AtomicBool, Ordering},
};
use std::sync::Arc;

use hardy_bpa::{
    // Aliased: `Error` is the builder's; the crate-root generic error is
    // what a conversion raises.
    Error as BpaError,
    async_trait,
    bpa::Bpa,
    builder::Error,
    cla::{self, Cla, ClaInit, Registration},
    node_ids::NodeIds,
    services::{self, Application, ApplicationSink, Service, ServiceSink, StatusNotify},
    stream::{Receiver, Segment},
};
use hardy_bpv7::{
    bundle::Id,
    eid::{DtnNodeId, Eid, IpnNodeId, NodeId, Service as ServiceId},
    status_report::ReasonCode,
};
use time::OffsetDateTime;

/// Records the declared cap `on_register` delivers.
struct RecordingCla {
    sink: hardy_async::sync::spin::Once<Box<dyn cla::Sink>>,
    cap_tx: flume::Sender<Option<NonZeroU64>>,
}

#[async_trait]
impl Cla for RecordingCla {
    async fn on_register(
        &self,
        sink: Box<dyn cla::Sink>,
        _node_ids: &[NodeId],
        max_bundle_size: Option<NonZeroU64>,
    ) {
        self.sink.call_once(|| sink);
        let _ = self.cap_tx.send(max_bundle_size);
    }

    async fn on_unregister(&self) {}

    async fn forward(
        &self,
        _lane: Option<u32>,
        _cla_addr: &cla::ClaAddress,
        _bundle_id: &Id,
        _total_len: u64,
        _stream: &mut dyn Receiver<Segment>,
    ) -> cla::Result<cla::ForwardBundleResult> {
        Ok(cla::ForwardBundleResult::Sent)
    }
}

/// A CLA crate's configuration, as a server would pass it to the builder:
/// it converts into a `Registration` declaring `max_bundle_size`, or
/// fails with its own error.
struct ClaConfig {
    max_bundle_size: Option<NonZeroU64>,
    fail: bool,
    converted: Arc<AtomicBool>,
    cap_tx: flume::Sender<Option<NonZeroU64>>,
}

#[derive(Debug, thiserror::Error)]
#[error("the configuration is invalid")]
struct ConfigError;

impl TryFrom<ClaConfig> for Registration {
    type Error = ConfigError;

    fn try_from(config: ClaConfig) -> Result<Self, Self::Error> {
        config.converted.store(true, Ordering::SeqCst);
        if config.fail {
            return Err(ConfigError);
        }
        let cla = Arc::new(RecordingCla {
            sink: hardy_async::sync::spin::Once::new(),
            cap_tx: config.cap_tx,
        });
        Ok(Registration::new(
            cla,
            ClaInit {
                max_bundle_size: config.max_bundle_size,
                ..Default::default()
            },
        ))
    }
}

fn cla_config(
    fail: bool,
) -> (
    ClaConfig,
    Arc<AtomicBool>,
    flume::Receiver<Option<NonZeroU64>>,
) {
    let converted = Arc::new(AtomicBool::new(false));
    let (cap_tx, cap_rx) = flume::bounded(1);
    (
        ClaConfig {
            max_bundle_size: NonZeroU64::new(4096),
            fail,
            converted: converted.clone(),
            cap_tx,
        },
        converted,
        cap_rx,
    )
}

/// Records its registration EID.
struct RecordingService {
    eid_tx: flume::Sender<Eid>,
}

#[async_trait]
impl Service for RecordingService {
    async fn on_register(&self, endpoint: &Eid, _sink: Box<dyn ServiceSink>) {
        let _ = self.eid_tx.send(endpoint.clone());
    }

    async fn on_unregister(&self) {}

    async fn on_deliver(
        &self,
        _bundle_id: &Id,
        _expiry: OffsetDateTime,
        _total_len: u64,
        _stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        Ok(())
    }

    async fn on_status_notify(
        &self,
        _bundle_id: &Id,
        _from: &Eid,
        _kind: StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<OffsetDateTime>,
    ) {
    }
}

/// Records its registration EID.
struct RecordingApp {
    eid_tx: flume::Sender<Eid>,
}

#[async_trait]
impl Application for RecordingApp {
    async fn on_register(&self, source: &Eid, _sink: Box<dyn ApplicationSink>) {
        let _ = self.eid_tx.send(source.clone());
    }

    async fn on_unregister(&self) {}

    async fn on_deliver(
        &self,
        _bundle_id: &Id,
        _expiry: OffsetDateTime,
        _ack_requested: bool,
        _total_len: u64,
        _stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        Ok(())
    }

    async fn on_status_notify(
        &self,
        _bundle_id: &Id,
        _from: &Eid,
        _kind: StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<OffsetDateTime>,
    ) {
    }
}

fn service() -> (Arc<dyn Service>, flume::Receiver<Eid>) {
    let (eid_tx, eid_rx) = flume::bounded(1);
    (Arc::new(RecordingService { eid_tx }), eid_rx)
}

fn ipn_node_ids() -> NodeIds {
    [NodeId::Ipn(IpnNodeId {
        allocator_id: 0,
        node_number: 7,
    })]
    .as_slice()
    .try_into()
    .unwrap()
}

// A configured CLA is converted by `build()`, not when it is configured,
// and its declarations reach `on_register` once the BPA starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cla_converts_at_build_and_registers_at_start() {
    let (config, converted, cap_rx) = cla_config(false);
    let builder = Bpa::builder().cla("configured", config, None);
    assert!(
        !converted.load(Ordering::SeqCst),
        "configuring a CLA must not convert it"
    );

    let bpa = builder.build().await.unwrap();
    assert!(converted.load(Ordering::SeqCst), "build() converts it");
    assert!(cap_rx.is_empty(), "nothing registers before start()");

    bpa.start(false).await;
    assert_eq!(
        cap_rx.try_recv().expect("start() registers the CLA"),
        NonZeroU64::new(4096),
        "the converted declarations reach on_register"
    );
    bpa.shutdown().await;
}

#[tokio::test]
async fn cla_conversion_failure_names_the_cla() {
    let (config, _, _) = cla_config(true);
    let Err(err) = Bpa::builder().cla("broken", config, None).build().await else {
        panic!("a failed conversion must fail build()");
    };
    let Error::ClaConversion { name, source } = &err else {
        panic!("expected ClaConversion, got {err:?}");
    };
    assert_eq!(name, "broken");
    assert!(source.is::<ConfigError>(), "{source:?}");
}

#[tokio::test]
async fn duplicate_cla_name_is_an_error_not_a_panic() {
    let (first, _, _) = cla_config(false);
    let (second, _, _) = cla_config(false);
    let Err(err) = Bpa::builder()
        .cla("same", first, None)
        .cla("same", second, None)
        .build()
        .await
    else {
        panic!("a duplicate name must fail build()");
    };
    assert!(
        matches!(
            &err,
            Error::Cla { name, source: cla::Error::AlreadyExists(dup) } if name == "same" && dup == "same"
        ),
        "{err:?}"
    );
}

// A service and an application both go live at start(), on the EIDs their
// service ids resolve to.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_and_application_register_at_start() {
    let (service, service_rx) = service();
    let (app_tx, app_rx) = flume::bounded(1);
    let app: Arc<dyn Application> = Arc::new(RecordingApp { eid_tx: app_tx });

    let bpa = Bpa::builder()
        .node_ids(ipn_node_ids())
        .service(ServiceId::Ipn(1), service)
        .application(ServiceId::Ipn(2), app)
        .build()
        .await
        .unwrap();
    assert!(service_rx.is_empty() && app_rx.is_empty());

    bpa.start(false).await;
    assert_eq!(
        service_rx.try_recv().unwrap(),
        "ipn:0.7.1".parse::<Eid>().unwrap()
    );
    assert_eq!(
        app_rx.try_recv().unwrap(),
        "ipn:0.7.2".parse::<Eid>().unwrap()
    );
    bpa.shutdown().await;
}

// A service and an application share one service-id space.
#[tokio::test]
async fn service_id_taken_by_an_application_is_refused() {
    let (service, _) = service();
    let (app_tx, _) = flume::bounded(1);
    let app: Arc<dyn Application> = Arc::new(RecordingApp { eid_tx: app_tx });

    let Err(err) = Bpa::builder()
        .node_ids(ipn_node_ids())
        .application(ServiceId::Ipn(1), app)
        .service(ServiceId::Ipn(1), service)
        .build()
        .await
    else {
        panic!("a taken service id must fail build()");
    };
    let Error::Service { service_id, source } = &err else {
        panic!("expected Service, got {err:?}");
    };
    assert_eq!(*service_id, ServiceId::Ipn(1));
    assert!(
        matches!(**source, services::Error::ServiceIdInUse(_)),
        "{source:?}"
    );
}

#[tokio::test]
async fn administrative_service_id_is_refused() {
    let (service, _) = service();
    let Err(err) = Bpa::builder()
        .service(ServiceId::Ipn(0), service)
        .build()
        .await
    else {
        panic!("the administrative endpoint must fail build()");
    };
    assert!(
        matches!(
            &err,
            Error::Service { service_id: ServiceId::Ipn(0), source }
                if matches!(**source, services::Error::AdministrativeEndpoint(_))
        ),
        "{err:?}"
    );
}

// A dtn service id on a node with only an ipn node id has no EID to
// register on.
#[tokio::test]
async fn service_id_without_a_node_id_for_its_scheme_is_refused() {
    let (service, _) = service();
    let Err(err) = Bpa::builder()
        .node_ids(ipn_node_ids())
        .service(ServiceId::Dtn("echo".into()), service)
        .build()
        .await
    else {
        panic!("an unresolvable service id must fail build()");
    };
    let Error::Service { service_id, source } = &err else {
        panic!("expected Service, got {err:?}");
    };
    assert_eq!(*service_id, ServiceId::Dtn("echo".into()));
    assert!(matches!(**source, services::Error::NodeId(_)), "{source:?}");
}

// The dtn scheme resolves once a dtn node id is configured.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dtn_service_id_resolves_against_a_dtn_node_id() {
    let (service, service_rx) = service();
    let node_ids: NodeIds = [NodeId::Dtn(DtnNodeId {
        node_name: "node".into(),
    })]
    .as_slice()
    .try_into()
    .unwrap();

    let bpa = Bpa::builder()
        .node_ids(node_ids)
        .service(ServiceId::Dtn("echo".into()), service)
        .build()
        .await
        .unwrap();
    bpa.start(false).await;
    assert_eq!(
        service_rx.try_recv().unwrap(),
        "dtn://node/echo".parse::<Eid>().unwrap()
    );
    bpa.shutdown().await;
}

// A service crate's configuration converts the same way a CLA's does, and
// its error is carried with the service id.
#[tokio::test]
async fn service_conversion_failure_names_the_service() {
    struct BrokenServiceConfig;

    impl TryFrom<BrokenServiceConfig> for Arc<dyn Service> {
        type Error = BpaError;

        fn try_from(_: BrokenServiceConfig) -> Result<Self, Self::Error> {
            Err(Box::new(ConfigError))
        }
    }

    let Err(err) = Bpa::builder()
        .node_ids(ipn_node_ids())
        .service(ServiceId::Ipn(9), BrokenServiceConfig)
        .build()
        .await
    else {
        panic!("a failed conversion must fail build()");
    };
    let Error::ServiceConversion { service_id, source } = &err else {
        panic!("expected ServiceConversion, got {err:?}");
    };
    assert_eq!(*service_id, ServiceId::Ipn(9));
    assert!(source.is::<ConfigError>(), "{source:?}");
}
