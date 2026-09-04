//! Annotation-slot registration through the public filter-pack API.

use core::num::NonZeroUsize;

use hardy_bpa::{
    bpa::Bpa,
    filter::{
        pack::{self, FilterPack},
        slots,
    },
};

fn bound(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}

#[tokio::test]
async fn duplicate_slot_names_in_one_pack_fail_build() {
    let mut pack = FilterPack::new("vendor");
    let _a = pack.annotation_slot::<u32>("x", bound(16));
    let _b = pack.annotation_slot::<u64>("x", bound(32));

    let Err(err) = Bpa::builder().add_filters(pack).build().await else {
        panic!("duplicate slot names must fail build()");
    };

    let Some(pack::Error::Slots(slots::Error::DuplicateName(name))) =
        err.downcast_ref::<pack::Error>()
    else {
        panic!("expected slots::Error::DuplicateName, got: {err}");
    };
    assert_eq!(&**name, "vendor.x");
}

#[tokio::test]
async fn same_named_packs_collide_on_a_shared_slot_name() {
    let mut first = FilterPack::new("vendor");
    let _a = first.annotation_slot::<u32>("x", bound(16));
    let mut second = FilterPack::new("vendor");
    let _b = second.annotation_slot::<u32>("x", bound(16));

    let Err(err) = Bpa::builder()
        .add_filters(first)
        .add_filters(second)
        .build()
        .await
    else {
        panic!("same-named packs sharing a slot name must fail build()");
    };

    let Some(pack::Error::Slots(slots::Error::DuplicateName(name))) =
        err.downcast_ref::<pack::Error>()
    else {
        panic!("expected slots::Error::DuplicateName, got: {err}");
    };
    assert_eq!(&**name, "vendor.x");
}

#[tokio::test]
async fn distinct_packs_may_share_a_slot_name() {
    let mut alice = FilterPack::new("alice");
    let _a = alice.annotation_slot::<u32>("x", bound(16));
    let mut bob = FilterPack::new("bob");
    let _b = bob.annotation_slot::<u32>("x", bound(16));

    let bpa = Bpa::builder()
        .add_filters(alice)
        .add_filters(bob)
        .build()
        .await
        .expect("pack-prefixed slot names must not collide across packs");
    bpa.shutdown().await;
}

#[tokio::test]
async fn distinct_slot_names_in_one_pack_build() {
    let mut pack = FilterPack::new("vendor");
    let _a = pack.annotation_slot::<u32>("x", bound(16));
    let _b = pack.annotation_slot::<u32>("y", bound(16));

    let bpa = Bpa::builder()
        .add_filters(pack)
        .build()
        .await
        .expect("distinct slot names must build");
    bpa.shutdown().await;
}

#[tokio::test]
async fn empty_pack_name_fails_build() {
    let Err(err) = Bpa::builder()
        .add_filters(FilterPack::new(""))
        .build()
        .await
    else {
        panic!("an empty pack name must fail build()");
    };

    let Some(pack::Error::EmptyName) = err.downcast_ref::<pack::Error>() else {
        panic!("expected pack::Error::EmptyName, got: {err}");
    };
}

#[tokio::test]
async fn dotted_pack_name_fails_build() {
    let Err(err) = Bpa::builder()
        .add_filters(FilterPack::new("ven.dor"))
        .build()
        .await
    else {
        panic!("a dotted pack name must fail build()");
    };

    let Some(pack::Error::DottedName(name)) = err.downcast_ref::<pack::Error>() else {
        panic!("expected pack::Error::DottedName, got: {err}");
    };
    assert_eq!(&**name, "ven.dor");
}
