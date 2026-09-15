// The typed-error discriminator's round trip, over the one payload
// whose encoding has a value it must refuse: a drop reason code.

use hardy_bpa::services;
use hardy_bpv7::status_report::ReasonCode;
use hardy_proto::status::{DETAIL_KEY, embed_service_error, recover_service_error};
use tonic::Status;

fn round_trip(reason: Option<ReasonCode>) -> Option<services::Error> {
    let e = services::Error::Dropped(reason);
    recover_service_error(&embed_service_error(Status::aborted(e.to_string()), &e))
}

#[test]
fn a_known_drop_reason_round_trips() {
    let Some(services::Error::Dropped(reason)) = round_trip(Some(ReasonCode::DepletedStorage))
    else {
        panic!("a drop reason must recover as a drop");
    };
    assert_eq!(reason, Some(ReasonCode::DepletedStorage));
}

#[test]
fn an_unassigned_drop_reason_round_trips() {
    // Unassigned codes are carried, so a peer running a later revision
    // of the registry is not flattened to "no reason given".
    let Some(services::Error::Dropped(reason)) = round_trip(Some(ReasonCode::Unassigned(200)))
    else {
        panic!("an unassigned drop reason must recover as a drop");
    };
    assert_eq!(reason, Some(ReasonCode::Unassigned(200)));
}

#[test]
fn the_reserved_drop_reason_is_not_carried() {
    // 255 is reserved: `ReasonCode::try_from` refuses it, so neither
    // direction may produce it. The drop still recovers, without a
    // reason.
    assert!(ReasonCode::try_from(255).is_err());
    let Some(services::Error::Dropped(reason)) = round_trip(Some(ReasonCode::Unassigned(255)))
    else {
        panic!("a reserved drop reason must still recover as a drop");
    };
    assert_eq!(reason, None);
}

#[test]
fn a_reserved_reason_from_a_foreign_server_does_not_recover() {
    // A peer that is not this crate can still put 255 on the wire.
    // Recovery refuses it rather than handing a local caller the one
    // `ReasonCode` bpv7 will not construct; the caller falls back to
    // the status code.
    let mut status = embed_service_error(
        Status::aborted("dropped"),
        &services::Error::Dropped(Some(ReasonCode::DepletedStorage)),
    );
    status
        .metadata_mut()
        .insert(DETAIL_KEY, "255".parse().unwrap());
    assert!(recover_service_error(&status).is_none());
}
