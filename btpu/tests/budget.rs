//! A retention budget shared between receivers and the CLA, through the
//! public `budget` and `receiver` APIs.

#![cfg(target_has_atomic = "ptr")]

mod common;

use std::{sync::Arc, thread};

use hardy_btpu::{
    budget::RetentionBudget,
    receiver::{
        Delivery, MaxRetainedBytes, Receiver, ReceiverConfig, ReceiverEvent, RejectReason,
        SEGMENT_OVERHEAD,
    },
};

use self::common::{cancel, none, receiver, receiver_config, rejected, segment};

const DATA: &[u8] = b"0123456789";

// What a receiver is charged for holding one segment of `DATA` given
// outside a PDU.
const CHARGE: usize = DATA.len() + SEGMENT_OVERHEAD;

fn budget(limit: usize) -> Arc<RetentionBudget> {
    Arc::new(RetentionBudget::new(
        MaxRetainedBytes::try_from(limit).unwrap(),
    ))
}

fn joined(budget: &Arc<RetentionBudget>) -> Receiver {
    receiver(16, 1024).with_budget(Arc::clone(budget))
}

#[test]
fn a_charge_is_released_when_dropped() {
    let budget = budget(100);
    let first = budget.try_charge(60).unwrap();
    assert!(budget.try_charge(41).is_none());
    let second = budget.try_charge(40).unwrap();
    assert_eq!((first.bytes(), second.bytes()), (60, 40));
    assert_eq!(budget.used(), 100);
    drop(first);
    assert_eq!(budget.used(), 40);
    drop(second);
    assert_eq!(budget.used(), 0);
}

#[test]
fn concurrent_charges_never_exceed_the_limit() {
    const LIMIT: usize = 1500;
    let budget = budget(LIMIT);
    let charges: Vec<_> = thread::scope(|s| {
        let workers: Vec<_> = (0..4)
            .map(|_| {
                s.spawn(|| {
                    (0..1000)
                        .filter_map(|_| budget.try_charge(1))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().unwrap())
            .collect()
    });
    assert_eq!(charges.len(), LIMIT);
    assert_eq!(budget.used(), LIMIT);
    drop(charges);
    assert_eq!(budget.used(), 0);
}

#[test]
fn receivers_sharing_a_budget_are_bounded_together() {
    let budget = budget(2 * CHARGE);
    let mut first = joined(&budget);
    let mut second = joined(&budget);
    assert_eq!(first.process_message(segment(0, 0, DATA)), none());
    assert_eq!(second.process_message(segment(0, 0, DATA)), none());
    assert_eq!(budget.used(), 2 * CHARGE);

    // Within its own limit, but not the budget's.
    assert_eq!(
        second.process_message(segment(1, 0, DATA)),
        vec![rejected(1, RejectReason::BudgetFull)]
    );
    assert_eq!(second.retained_bytes(), CHARGE);
    assert_eq!(budget.used(), 2 * CHARGE);

    // Dropping a receiver returns its share.
    drop(first);
    assert_eq!(budget.used(), CHARGE);
    assert_eq!(second.process_message(segment(2, 0, DATA)), none());
    assert_eq!(budget.used(), 2 * CHARGE);
}

#[test]
fn receiver_full_is_reported_before_budget_full() {
    let budget = budget(CHARGE);
    let mut r = Receiver::new(ReceiverConfig {
        max_retained_bytes: Some(MaxRetainedBytes::try_from(CHARGE).unwrap()),
        ..receiver_config(16, 1024)
    })
    .with_budget(Arc::clone(&budget));
    assert_eq!(r.process_message(segment(0, 0, DATA)), none());
    assert_eq!(
        r.process_message(segment(1, 0, DATA)),
        vec![rejected(1, RejectReason::ReceiverFull)]
    );
    assert_eq!(budget.used(), CHARGE);
}

#[test]
fn a_cla_charge_leaves_less_for_receivers() {
    let budget = budget(2 * CHARGE);
    let mut r = joined(&budget);
    let charge = budget.try_charge(CHARGE + 1).unwrap();
    assert_eq!(
        r.process_message(segment(0, 0, DATA)),
        vec![rejected(0, RejectReason::BudgetFull)]
    );
    drop(charge);
    assert_eq!(r.process_message(segment(1, 0, DATA)), none());
}

#[test]
fn joining_a_budget_charges_what_is_already_held() {
    let earlier = budget(1024);
    let mut r = joined(&earlier);
    r.process_message(segment(0, 0, DATA));
    assert_eq!(earlier.used(), CHARGE);

    // Charged whatever the limit, and moved off the earlier budget.
    let small = budget(1);
    let mut r = r.with_budget(Arc::clone(&small));
    assert_eq!((earlier.used(), small.used()), (0, CHARGE));
    assert_eq!(
        r.process_message(segment(1, 0, DATA)),
        vec![rejected(1, RejectReason::BudgetFull)]
    );

    // What was held over the limit is released as it leaves.
    r.process_message(cancel(0));
    assert_eq!(small.used(), 0);
}

#[test]
fn a_reset_releases_the_receivers_share() {
    let budget = budget(1024);
    let mut r = joined(&budget);
    r.process_message(segment(0, 0, DATA));
    r.process_message(segment(1, 0, DATA));
    assert_eq!(budget.used(), 2 * CHARGE);
    r.reset();
    assert_eq!(budget.used(), 0);
}

#[test]
fn released_segments_are_not_charged() {
    let budget = budget(1024);
    let mut r = Receiver::new(ReceiverConfig {
        delivery: Delivery::Streamed,
        ..receiver_config(16, 1024)
    })
    .with_budget(Arc::clone(&budget));
    r.process_message(segment(0, 0, DATA));
    assert_eq!(budget.used(), 0);
    r.process_message(segment(0, 2, DATA));
    assert_eq!(budget.used(), CHARGE);
    r.process_message(segment(0, 1, DATA));
    assert_eq!(budget.used(), 0);
}

#[test]
fn a_segment_released_as_it_arrives_needs_no_room_in_the_budget() {
    // One stored segment leaves less than one segment's bookkeeping free.
    let budget = budget(CHARGE + SEGMENT_OVERHEAD - 1);
    let mut whole = joined(&budget);
    assert_eq!(whole.process_message(segment(0, 2, DATA)), none());
    assert_eq!(budget.used(), CHARGE);

    let mut streamed = Receiver::new(ReceiverConfig {
        delivery: Delivery::Streamed,
        ..receiver_config(16, 1024)
    })
    .with_budget(Arc::clone(&budget));
    for index in 0..2 {
        let events = streamed.process_message(segment(0, index, DATA));
        let Some(ReceiverEvent::TransferData { data, .. }) = events.last() else {
            panic!("segment {index} is released, not refused: {events:?}");
        };
        assert_eq!(&data[..], DATA);
        assert_eq!(streamed.retained_bytes(), 0);
        assert_eq!(budget.used(), CHARGE);
    }
}
