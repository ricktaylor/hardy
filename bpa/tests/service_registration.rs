//! Integration tests for claiming a service id: the id is taken before
//! the factory runs, so a service is never built under an id another
//! registration already holds, and an id a failed build reserved is free
//! again.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use hardy_bpa::{
    async_trait,
    bpa::Bpa,
    service::{self, Context, Factory, RegistrarExt, Service, StatusNotify, factory_fn},
    stream::{Receiver, Segment},
};
use hardy_bpv7::{
    bundle::Id,
    eid::{Eid, Service as ServiceId},
    status_report::ReasonCode,
};

/// Records that it was built, so a test can tell whether the factory ran
/// at all.
struct TestService;

impl TestService {
    fn factory(built: Arc<AtomicBool>) -> impl Factory<Component = Self> {
        factory_fn(move |_cx: Context| {
            built.store(true, Ordering::Relaxed);
            Ok(Arc::new(Self))
        })
    }
}

#[async_trait]
impl Service for TestService {
    async fn deliver(
        &self,
        _bundle_id: &Id,
        _expiry: time::OffsetDateTime,
        _ack_requested: bool,
        _total_len: u64,
        _stream: &mut dyn Receiver<Segment>,
    ) -> service::Result<()> {
        Ok(())
    }

    async fn report_status(
        &self,
        _bundle_id: &Id,
        _from: &Eid,
        _kind: StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<time::OffsetDateTime>,
    ) {
    }
}

// An id another service already holds is refused before anything is
// built: the second factory never runs.
#[tokio::test]
async fn a_taken_service_id_is_not_built() {
    let bpa = Bpa::builder().build().await.unwrap();
    assert!(bpa.start(false).await.is_empty());

    let first = Arc::new(AtomicBool::new(false));
    bpa.register_service(
        Some(ServiceId::Ipn(42)),
        TestService::factory(first.clone()),
    )
    .await
    .expect("The first registration should succeed");
    assert!(first.load(Ordering::Relaxed));

    let second = Arc::new(AtomicBool::new(false));
    let Err(err) = bpa
        .register_service(
            Some(ServiceId::Ipn(42)),
            TestService::factory(second.clone()),
        )
        .await
    else {
        panic!("A second service claiming the id should be refused");
    };
    assert!(matches!(err, service::Error::ServiceIdInUse(_)));
    assert!(
        !second.load(Ordering::Relaxed),
        "The id was already taken, so the factory should not have run"
    );

    bpa.shutdown().await;
}

// A factory that fails leaves nothing behind, so the id it reserved is
// free for the next attempt.
#[tokio::test]
async fn a_failed_build_releases_the_service_id() {
    let bpa = Bpa::builder().build().await.unwrap();
    assert!(bpa.start(false).await.is_empty());

    let Err(err) = bpa
        .register_service(
            Some(ServiceId::Ipn(42)),
            factory_fn(|_cx: Context| -> service::Result<Arc<TestService>> {
                Err(service::Error::Internal("The factory refused".into()))
            }),
        )
        .await
    else {
        panic!("A factory that fails should fail the registration");
    };
    assert!(matches!(err, service::Error::Internal(_)));

    let built = Arc::new(AtomicBool::new(false));
    bpa.register_service(Some(ServiceId::Ipn(42)), TestService::factory(built))
        .await
        .expect("The refused id should be free again");

    bpa.shutdown().await;
}
