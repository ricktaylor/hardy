//! Integration tests for the size-cap negotiation at CLA registration: the
//! effective cap delivered to `Cla::on_register` is the minimum of the
//! BPA's configured policy and the CLA's declared limit, clamped to the
//! target's addressable bound.

use core::num::NonZeroU64;
use std::sync::Arc;

use hardy_bpa::{
    async_trait,
    bpa::{Bpa, BpaRegistration},
    builder::BpaBuilder,
    cla::{self, Cla, ClaInit},
    stream::{Receiver, Segment},
};
use hardy_bpv7::eid::NodeId;

/// Records the effective cap `on_register` delivers.
struct CapCla {
    sink: hardy_async::sync::spin::Once<Box<dyn cla::Sink>>,
    cap_tx: flume::Sender<Option<NonZeroU64>>,
}

impl CapCla {
    fn new() -> (Arc<Self>, flume::Receiver<Option<NonZeroU64>>) {
        let (tx, rx) = flume::bounded(1);
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                cap_tx: tx,
            }),
            rx,
        )
    }
}

#[async_trait]
impl Cla for CapCla {
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
        _bundle_id: &hardy_bpv7::bundle::Id,
        _total_len: u64,
        _stream: &mut dyn Receiver<Segment>,
    ) -> cla::Result<cla::ForwardBundleResult> {
        Ok(cla::ForwardBundleResult::Sent)
    }
}

// `on_register` completes before `register_cla` returns, so the recorded
// cap is already in the channel — no waiting, no timing.
async fn negotiate(builder: BpaBuilder, declared: Option<NonZeroU64>) -> (Bpa, Option<NonZeroU64>) {
    let bpa = builder.build().await.unwrap();
    bpa.start(false).await;

    let (cla, cap_rx) = CapCla::new();
    bpa.register_cla(
        "cap".to_string(),
        cla,
        None,
        ClaInit {
            max_bundle_size: declared,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let effective = cap_rx.try_recv().expect("on_register was not called");
    (bpa, effective)
}

const BPA_CAP: NonZeroU64 = NonZeroU64::new(64 * 1024).unwrap();

/// A declaration below the BPA's cap is the effective cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn declared_limit_below_bpa_cap_wins() {
    let declared = NonZeroU64::new(BPA_CAP.get() / 2).unwrap();
    let (bpa, effective) = negotiate(Bpa::builder().max_bundle_size(BPA_CAP), Some(declared)).await;
    assert_eq!(effective, Some(declared));
    bpa.shutdown().await;
}

/// A declaration above the BPA's cap folds down to the BPA's cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bpa_cap_wins_over_larger_declaration() {
    let declared = NonZeroU64::new(BPA_CAP.get() * 2).unwrap();
    let (bpa, effective) = negotiate(Bpa::builder().max_bundle_size(BPA_CAP), Some(declared)).await;
    assert_eq!(effective, Some(BPA_CAP));
    bpa.shutdown().await;
}

/// No declaration: the effective cap is the BPA's own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_declaration_receives_bpa_cap() {
    let (bpa, effective) = negotiate(Bpa::builder().max_bundle_size(BPA_CAP), None).await;
    assert_eq!(effective, Some(BPA_CAP));
    bpa.shutdown().await;
}

/// A configured cap beyond the target's addressable bound is clamped to it
/// at construction, and the clamped value is what registration advertises —
/// the advertised and enforced caps always agree.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn over_addressable_cap_is_clamped_before_advertising() {
    let (bpa, effective) = negotiate(Bpa::builder().max_bundle_size(NonZeroU64::MAX), None).await;
    assert_eq!(effective, Some(NonZeroU64::new(isize::MAX as u64).unwrap()));
    bpa.shutdown().await;
}
