//! Integration tests for `CreationTimestamp` issuance via the public
//! `hardy_bpv7` API.

use std::{collections::BTreeSet, thread};

use hardy_bpv7::creation_timestamp::CreationTimestamp;

// Every pair `now()` issues is strictly greater than the one before —
// same-millisecond calls take ascending sequence numbers — so ids built from
// consecutive calls can never collide, without the caller checking. A
// stateless scheme (the clock alone) fails this the moment two reads land on
// the same instant.
#[test]
fn now_issues_strictly_monotonic_pairs() {
    let mut last = CreationTimestamp::now();
    for _ in 0..10_000 {
        let next = CreationTimestamp::now();
        assert!(
            next > last,
            "every issued pair must exceed its predecessor: {next:?} !> {last:?}"
        );
        last = next;
    }
}

// Concurrent issuers never receive the same pair: uniqueness is the atomic's
// guarantee, not a property of clock resolution.
#[test]
fn now_never_issues_the_same_pair_twice_across_threads() {
    let issued: Vec<CreationTimestamp> = thread::scope(|s| {
        let handles: Vec<_> = (0..4)
            .map(|_| {
                s.spawn(|| {
                    (0..10_000)
                        .map(|_| CreationTimestamp::now())
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("issuer thread panicked"))
            .collect()
    });

    let total = issued.len();
    let unique: BTreeSet<_> = issued.into_iter().collect();
    assert_eq!(unique.len(), total, "an issued pair was duplicated");
}
