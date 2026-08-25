//! Annotation-slot registration through the public builder API.

use core::num::NonZeroUsize;

use hardy_bpa::{bpa::Bpa, filter::slots};

fn bound(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}

#[tokio::test]
async fn duplicate_slot_names_fail_build() {
    let (builder, _a) = Bpa::builder().annotation_slot::<u32>("vendor.x", bound(16));
    let (builder, _b) = builder.annotation_slot::<u64>("vendor.x", bound(32));

    let Err(err) = builder.build().await else {
        panic!("duplicate slot names must fail build()");
    };

    let Some(slots::Error::DuplicateName(name)) = err.downcast_ref::<slots::Error>() else {
        panic!("expected slots::Error::DuplicateName, got: {err}");
    };
    assert_eq!(&**name, "vendor.x");
}

#[tokio::test]
async fn distinct_slot_names_build() {
    let (builder, _a) = Bpa::builder().annotation_slot::<u32>("vendor.x", bound(16));
    let (builder, _b) = builder.annotation_slot::<u32>("vendor.y", bound(16));

    let bpa = builder
        .build()
        .await
        .expect("distinct slot names must build");
    bpa.shutdown().await;
}
