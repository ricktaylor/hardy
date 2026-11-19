//! Integration tests for the size-cap negotiation at CLA attachment: the
//! receive limit settled in `Terms` is the minimum of the BPA's configured
//! policy and the CLA's declared limit, clamped to the target's
//! addressable bound.

use core::num::NonZeroU64;
use std::sync::Arc;

use hardy_bpa::{
    async_trait,
    bpa::Bpa,
    builder::BpaBuilder,
    cla::{self, Cla, Context, Factory, RegistrarExt, Request, Terms, factory_fn},
    stream::{Receiver, Segment},
};

/// Keeps the terms it was built under.
struct CapCla {
    terms: Terms,
}

impl CapCla {
    fn factory(declared: Option<NonZeroU64>) -> impl Factory<Component = Self> {
        let request = Request {
            max_bundle_size: declared,
            ..Default::default()
        };
        factory_fn(request, |cx: Context| {
            Ok(Arc::new(Self { terms: cx.terms }))
        })
    }
}

#[async_trait]
impl Cla for CapCla {
    async fn forward(
        &self,
        _lane: Option<u32>,
        _cla_addr: &cla::Address,
        _bundle_id: &hardy_bpv7::bundle::Id,
        _total_len: u64,
        _stream: &mut dyn Receiver<Segment>,
    ) -> cla::Result<cla::ForwardOutcome> {
        Ok(cla::ForwardOutcome::Sent)
    }
}

// The terms are settled before the CLA is built, so the limit it holds is
// the negotiated one.
async fn negotiate(builder: BpaBuilder, declared: Option<NonZeroU64>) -> (Bpa, Option<NonZeroU64>) {
    let bpa = builder.build();
    bpa.start(false).await.unwrap();

    let cla = bpa
        .register_cla("cap", CapCla::factory(declared))
        .await
        .unwrap();
    let effective = cla.terms.max_bundle_size;
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
/// at construction, and the clamped value is what attachment settles on:
/// the advertised and enforced caps always agree.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn over_addressable_cap_is_clamped_before_advertising() {
    let (bpa, effective) = negotiate(Bpa::builder().max_bundle_size(NonZeroU64::MAX), None).await;
    assert_eq!(effective, Some(NonZeroU64::new(isize::MAX as u64).unwrap()));
    bpa.shutdown().await;
}
