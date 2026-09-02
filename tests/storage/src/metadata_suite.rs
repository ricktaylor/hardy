use super::*;
use fixtures;

// ---------------------------------------------------------------------------
// Suite A: Basic CRUD Operations
// ---------------------------------------------------------------------------

/// META-01: Insert & Get
pub async fn meta_01_insert_and_get(store: Arc<dyn MetadataStorage>) {
    let bundle = fixtures::random_bundle();
    assert!(
        store.insert(&bundle).await.unwrap(),
        "insert should return true"
    );

    let got = store.get(bundle.id()).await.unwrap();
    let got = got.expect("get should return Some after insert");

    assert_eq!(got.id(), bundle.id());
    assert_eq!(got.status, bundle.status);
}

/// META-02: Duplicate Insert
pub async fn meta_02_duplicate_insert(store: Arc<dyn MetadataStorage>) {
    let bundle = fixtures::random_bundle();
    assert!(
        store.insert(&bundle).await.unwrap(),
        "first insert should return true"
    );
    assert!(
        !store.insert(&bundle).await.unwrap(),
        "second insert should return false"
    );
}

/// META-03: Update status
pub async fn meta_03_update_status(store: Arc<dyn MetadataStorage>) {
    let mut bundle = fixtures::random_bundle();
    bundle.status = BundleStatus::Waiting;
    assert!(store.insert(&bundle).await.unwrap());

    store
        .update_status(bundle.id(), &BundleStatus::Dispatching)
        .await
        .unwrap();

    let got = store.get(bundle.id()).await.unwrap().unwrap();
    assert_eq!(got.status, BundleStatus::Dispatching);
}

/// META-04: Tombstone
pub async fn meta_04_tombstone(store: Arc<dyn MetadataStorage>) {
    let bundle = fixtures::random_bundle();
    assert!(store.insert(&bundle).await.unwrap());

    store.tombstone(bundle.id()).await.unwrap();

    let got = store.get(bundle.id()).await.unwrap();
    assert!(got.is_none(), "get should return None after tombstone");

    assert!(
        !store.insert(&bundle).await.unwrap(),
        "insert after tombstone should return false (prevents resurrection)"
    );
}

/// META-05: Confirm Exists (recovery protocol)
///
/// Tests the startup recovery flow: bundles inserted before recovery are
/// marked unconfirmed by `start_recovery()`, then selectively confirmed
/// via `confirm_exists()`. Only applicable to persistent backends.
pub async fn meta_05_confirm_exists(store: Arc<dyn MetadataStorage>) {
    let bundle = fixtures::ingress_bundle();
    let missing_id = fixtures::random_bundle().id().clone();

    // Simulate a previous session: bundle already exists in the store
    assert!(store.insert(&bundle).await.unwrap());

    // Start recovery — marks all existing entries as unconfirmed
    store.start_recovery().await;

    // Confirm the bundle we know about — the recovered record must be the
    // inserted one, metadata body and status alike.
    let (metadata, status) = store
        .confirm_exists(bundle.id())
        .await
        .unwrap()
        .expect("confirm_exists should return Some for existing bundle");
    assert_eq!(metadata, bundle.metadata, "metadata should round-trip");
    assert_eq!(status, bundle.status, "status should round-trip");

    // A bundle ID that was never inserted should return None
    let missing = store.confirm_exists(&missing_id).await.unwrap();
    assert!(
        missing.is_none(),
        "confirm_exists should return None for missing bundle"
    );

    // The confirmed bundle should survive remove_unconfirmed
    let sink = super::VecSink::<bundle::Bundle>::new();
    store.remove_unconfirmed(&sink).await.unwrap();
    let removed = sink.into_inner();
    assert!(removed.is_empty(), "confirmed bundle should not be removed");

    // The confirmed bundle should still be retrievable
    let got = store.get(bundle.id()).await.unwrap();
    assert!(
        got.is_some(),
        "confirmed bundle should still exist after remove_unconfirmed"
    );
}

/// META-15: Metadata body round-trip
///
/// The serialized record body is otherwise never asserted: the suite's other
/// tests check only the bundle ID and status. Store a bundle whose every
/// metadata group is populated (`Origin::Ingress` provenance plus decoded
/// extension fields) and assert the whole record survives a store/load.
pub async fn meta_15_metadata_roundtrip(store: Arc<dyn MetadataStorage>) {
    let bundle = fixtures::ingress_bundle();
    assert!(store.insert(&bundle).await.unwrap());

    let got = store
        .get(bundle.id())
        .await
        .unwrap()
        .expect("get should return Some after insert");

    assert_eq!(got.bpv7, bundle.bpv7, "wire bundle should round-trip");
    assert_eq!(
        got.metadata, bundle.metadata,
        "metadata body (origin, received_at, extension fields) should round-trip"
    );
    assert_eq!(got.status, bundle.status, "status should round-trip");
}

// ---------------------------------------------------------------------------
// Suite B: Polling & Ordering
// ---------------------------------------------------------------------------

/// META-06: Poll Waiting (FIFO)
pub async fn meta_06_poll_waiting_fifo(store: Arc<dyn MetadataStorage>) {
    let now = time::OffsetDateTime::now_utc();
    let earlier = now - time::Duration::seconds(100);
    let later = now + time::Duration::seconds(100);

    let bundle_a = fixtures::bundle_with_status(BundleStatus::Waiting, earlier);
    let bundle_b = fixtures::bundle_with_status(BundleStatus::Waiting, later);

    // Insert in reverse order to ensure ordering is by received_at, not insertion
    assert!(store.insert(&bundle_b).await.unwrap());
    assert!(store.insert(&bundle_a).await.unwrap());

    let sink = super::VecSink::<bundle::Bundle>::new();
    store.poll_waiting(&sink).await.unwrap();
    let results = sink.into_inner();

    assert_eq!(results.len(), 2, "should return both Waiting bundles");
    assert_eq!(
        results[0].id(),
        bundle_a.id(),
        "first should be earlier bundle"
    );
    assert_eq!(
        results[1].id(),
        bundle_b.id(),
        "second should be later bundle"
    );
}

/// META-07: Poll Expiry
pub async fn meta_07_poll_expiry(store: Arc<dyn MetadataStorage>) {
    let now = time::OffsetDateTime::now_utc();

    // Bundle A: expiry = now + 500s (Waiting — should be included)
    let bundle_a = fixtures::bundle_with_expiry(
        BundleStatus::Waiting,
        now,
        core::time::Duration::from_secs(500),
    );
    // Bundle B: expiry = now + 300s (Waiting — should be included, returned first)
    let bundle_b = fixtures::bundle_with_expiry(
        BundleStatus::Waiting,
        now,
        core::time::Duration::from_secs(300),
    );
    // Bundle C: expiry = now + 100s (New — should be excluded)
    let bundle_c =
        fixtures::bundle_with_expiry(BundleStatus::New, now, core::time::Duration::from_secs(100));
    // Bundles D and E: the in-flight hand-off states. `New` is the ONLY
    // status poll_expiry may exclude — hand-off deferral is the reaper's
    // decision, made against fresh status at expiry time, so a backend that
    // pre-filters these hides them from the caller's policy.
    let bundle_d = fixtures::bundle_with_expiry(
        BundleStatus::ForwardAckPending { peer: 7 },
        now,
        core::time::Duration::from_secs(200),
    );
    let bundle_e = fixtures::bundle_with_expiry(
        BundleStatus::DeliveryAckPending {
            service: "ipn:60.3".parse().unwrap(),
        },
        now,
        core::time::Duration::from_secs(400),
    );

    assert!(store.insert(&bundle_a).await.unwrap());
    assert!(store.insert(&bundle_b).await.unwrap());
    assert!(store.insert(&bundle_c).await.unwrap());
    assert!(store.insert(&bundle_d).await.unwrap());
    assert!(store.insert(&bundle_e).await.unwrap());

    // Full poll: D, B, E, A in expiry order — C (New) excluded, both
    // hand-off states included
    let sink = super::VecSink::<bundle::Bundle>::new();
    store.poll_expiry(&sink, 10).await.unwrap();
    let results = sink.into_inner();

    assert_eq!(results.len(), 4, "only the New-status bundle is excluded");
    assert_eq!(
        results[0].id(),
        bundle_d.id(),
        "ForwardAckPending must be returned, in expiry order"
    );
    assert_eq!(
        results[1].id(),
        bundle_b.id(),
        "Waiting bundles follow in expiry order"
    );
    assert_eq!(
        results[2].id(),
        bundle_e.id(),
        "DeliveryAckPending must be returned, in expiry order"
    );
    assert_eq!(results[3].id(), bundle_a.id(), "latest expiry comes last");

    // Limit test: limit=1 should return only the earliest-expiry bundle
    let sink = super::VecSink::<bundle::Bundle>::new();
    store.poll_expiry(&sink, 1).await.unwrap();
    let results = sink.into_inner();

    assert_eq!(results.len(), 1, "limit=1 should return exactly 1 bundle");
    assert_eq!(results[0].id(), bundle_d.id());
}

/// META-08: Poll Pending (FIFO & Limit)
pub async fn meta_08_poll_pending_limit(store: Arc<dyn MetadataStorage>) {
    let now = time::OffsetDateTime::now_utc();
    let earlier = now - time::Duration::seconds(100);
    let later = now + time::Duration::seconds(100);

    // The assignment records carry distinct per-bundle adjacencies; the
    // poll key names the queue only (peer + queue), so both must match it
    // and each must come back with its own recorded adjacency.
    let status_a = BundleStatus::ForwardPending {
        peer: 42,
        queue: 0,
        next_hop: "ipn:100.0".parse().unwrap(),
    };
    let status_b = BundleStatus::ForwardPending {
        peer: 42,
        queue: 0,
        next_hop: "ipn:200.0".parse().unwrap(),
    };
    let status = BundleStatus::ForwardPending {
        peer: 42,
        queue: 0,
        next_hop: hardy_bpv7::eid::Eid::Null,
    };

    let bundle_a = fixtures::bundle_with_status(status_a.clone(), earlier);
    let bundle_b = fixtures::bundle_with_status(status_b.clone(), later);

    assert!(store.insert(&bundle_a).await.unwrap());
    assert!(store.insert(&bundle_b).await.unwrap());

    // limit=1: should return only the first (earlier) bundle
    let sink = super::VecSink::<bundle::Bundle>::new();
    store.poll_pending(&sink, &status, 1).await.unwrap();
    let results = sink.into_inner();

    assert_eq!(results.len(), 1, "limit=1 should return exactly 1 bundle");
    assert_eq!(
        results[0].id(),
        bundle_a.id(),
        "should be FIFO (earlier first)"
    );
    assert_eq!(
        results[0].status, status_a,
        "the record's own adjacency must survive the poll"
    );

    // limit=2: should return both in FIFO order
    let sink = super::VecSink::<bundle::Bundle>::new();
    store.poll_pending(&sink, &status, 2).await.unwrap();
    let results = sink.into_inner();

    assert_eq!(results.len(), 2, "limit=2 should return both bundles");
    assert_eq!(results[0].id(), bundle_a.id(), "first should be earlier");
    assert_eq!(results[1].id(), bundle_b.id(), "second should be later");
    assert_eq!(
        results[1].status, status_b,
        "the record's own adjacency must survive the poll"
    );
}

/// META-09: Poll Pending (Exact Match)
pub async fn meta_09_poll_pending_exact_match(store: Arc<dyn MetadataStorage>) {
    let now = time::OffsetDateTime::now_utc();

    let next_hop: hardy_bpv7::eid::Eid = "ipn:7.0".parse().unwrap();
    let status_a = BundleStatus::ForwardPending {
        peer: 1,
        queue: 0,
        next_hop: next_hop.clone(),
    };
    let status_b = BundleStatus::ForwardPending {
        peer: 2,
        queue: 0,
        next_hop: next_hop.clone(),
    };
    let status_c = BundleStatus::ForwardPending {
        peer: 1,
        queue: 1,
        next_hop,
    };

    let bundle_a = fixtures::bundle_with_status(status_a.clone(), now);
    let bundle_b = fixtures::bundle_with_status(status_b, now);
    let bundle_c = fixtures::bundle_with_status(status_c, now);

    assert!(store.insert(&bundle_a).await.unwrap());
    assert!(store.insert(&bundle_b).await.unwrap());
    assert!(store.insert(&bundle_c).await.unwrap());

    let sink = super::VecSink::<bundle::Bundle>::new();
    store.poll_pending(&sink, &status_a, 10).await.unwrap();
    let results = sink.into_inner();

    assert_eq!(
        results.len(),
        1,
        "only exact-matching status should be returned"
    );
    assert_eq!(results[0].id(), bundle_a.id());
}

/// META-10: Poll Fragments
pub async fn meta_10_poll_adu_fragments(store: Arc<dyn MetadataStorage>) {
    let source: hardy_bpv7::eid::Eid = "ipn:10.0".parse().unwrap();
    let timestamp = CreationTimestamp::now();

    let status = BundleStatus::AduFragment {
        source: source.clone(),
        timestamp: timestamp.clone(),
    };

    let bundle_a = fixtures::bundle_with_fragment(status.clone(), 0, 200);
    let bundle_b = fixtures::bundle_with_fragment(status.clone(), 100, 200);

    // Insert in reverse offset order
    assert!(store.insert(&bundle_b).await.unwrap());
    assert!(store.insert(&bundle_a).await.unwrap());

    let sink = super::VecSink::<bundle::Bundle>::new();
    store.poll_adu_fragments(&sink, &status).await.unwrap();
    let results = sink.into_inner();

    assert_eq!(results.len(), 2, "should return both fragments");
    assert_eq!(
        results[0]
            .bpv7
            .primary
            .id
            .fragment_info
            .as_ref()
            .unwrap()
            .offset,
        0,
        "first should be offset=0"
    );
    assert_eq!(
        results[1]
            .bpv7
            .primary
            .id
            .fragment_info
            .as_ref()
            .unwrap()
            .offset,
        100,
        "second should be offset=100"
    );
}

/// META-14: Poll Service Waiting (FIFO & filtering by service)
pub async fn meta_14_poll_service_waiting(store: Arc<dyn MetadataStorage>) {
    let now = time::OffsetDateTime::now_utc();
    let earlier = now - time::Duration::seconds(100);
    let later = now + time::Duration::seconds(100);

    let service_a: hardy_bpv7::eid::Eid = "ipn:50.1".parse().unwrap();
    let service_b: hardy_bpv7::eid::Eid = "ipn:50.2".parse().unwrap();

    let status_a = BundleStatus::WaitingForService {
        service: service_a.clone(),
    };
    let status_b = BundleStatus::WaitingForService {
        service: service_b.clone(),
    };

    // Two bundles for service_a at different times, one for service_b
    let bundle_a1 = fixtures::bundle_with_status(status_a.clone(), later);
    let bundle_a2 = fixtures::bundle_with_status(status_a.clone(), earlier);
    let bundle_b1 = fixtures::bundle_with_status(status_b, now);

    // Insert in non-FIFO order
    assert!(store.insert(&bundle_a1).await.unwrap());
    assert!(store.insert(&bundle_b1).await.unwrap());
    assert!(store.insert(&bundle_a2).await.unwrap());

    // Poll for service_a — should return both in FIFO order (earlier first)
    let sink = super::VecSink::<bundle::Bundle>::new();
    store
        .poll_service_waiting(service_a.clone(), &sink)
        .await
        .unwrap();
    let results = sink.into_inner();

    assert_eq!(results.len(), 2, "should return both bundles for service_a");
    assert_eq!(
        results[0].id(),
        bundle_a2.id(),
        "first should be earlier bundle"
    );
    assert_eq!(
        results[0].status, status_a,
        "returned bundle should have correct WaitingForService status"
    );
    assert_eq!(
        results[1].id(),
        bundle_a1.id(),
        "second should be later bundle"
    );

    // Poll for service_b — should return only the one matching bundle
    let sink = super::VecSink::<bundle::Bundle>::new();
    store.poll_service_waiting(service_b, &sink).await.unwrap();
    let results = sink.into_inner();

    assert_eq!(results.len(), 1, "should return only bundle for service_b");
    assert_eq!(results[0].id(), bundle_b1.id());
}

// ---------------------------------------------------------------------------
// Suite C: State Transitions & Bulk Ops
// ---------------------------------------------------------------------------

/// META-11: Reset Peer Queue
pub async fn meta_11_reset_peer_queue(store: Arc<dyn MetadataStorage>) {
    let now = time::OffsetDateTime::now_utc();

    let status_100 = BundleStatus::ForwardPending {
        peer: 100,
        queue: 0,
        next_hop: "ipn:100.0".parse().unwrap(),
    };
    let status_200 = BundleStatus::ForwardPending {
        peer: 200,
        queue: 0,
        next_hop: "ipn:200.0".parse().unwrap(),
    };

    let bundle_a = fixtures::bundle_with_status(status_100, now);
    let bundle_b = fixtures::bundle_with_status(status_200.clone(), now);

    assert!(store.insert(&bundle_a).await.unwrap());
    assert!(store.insert(&bundle_b).await.unwrap());

    let changed = store.reset_peer_queue(100).await.unwrap();
    assert_eq!(
        changed, 1,
        "reset_peer_queue should return 1 when bundles were reset"
    );

    let got_a = store.get(bundle_a.id()).await.unwrap().unwrap();
    assert_eq!(
        got_a.status,
        BundleStatus::Waiting,
        "peer 100 bundle should become Waiting"
    );

    let got_b = store.get(bundle_b.id()).await.unwrap().unwrap();
    assert_eq!(
        got_b.status, status_200,
        "peer 200 bundle should remain ForwardPending"
    );
}

/// META-16: Reset Service Queue (the unregistration sweep)
pub async fn meta_16_reset_service_queue(store: Arc<dyn MetadataStorage>) {
    let now = time::OffsetDateTime::now_utc();

    let service_a: hardy_bpv7::eid::Eid = "ipn:60.1".parse().unwrap();
    let service_b: hardy_bpv7::eid::Eid = "ipn:60.2".parse().unwrap();

    // One bundle queued for service_a, one in-flight with service_a, one
    // queued for service_b
    let queued_a = fixtures::bundle_with_status(
        BundleStatus::DeliverPending {
            service: service_a.clone(),
        },
        now,
    );
    let in_flight_a = fixtures::bundle_with_status(
        BundleStatus::DeliveryAckPending {
            service: service_a.clone(),
        },
        now,
    );
    let queued_b = fixtures::bundle_with_status(
        BundleStatus::DeliverPending {
            service: service_b.clone(),
        },
        now,
    );

    assert!(store.insert(&queued_a).await.unwrap());
    assert!(store.insert(&in_flight_a).await.unwrap());
    assert!(store.insert(&queued_b).await.unwrap());

    let changed = store.reset_service_queue(&service_a).await.unwrap();
    assert_eq!(changed, 1, "only service_a's queued bundle is swept");

    // The swept bundle now polls as WaitingForService for service_a
    let sink = super::VecSink::<bundle::Bundle>::new();
    store
        .poll_service_waiting(service_a.clone(), &sink)
        .await
        .unwrap();
    let results = sink.into_inner();
    assert_eq!(
        results.len(),
        1,
        "swept bundle should await re-registration"
    );
    assert_eq!(results[0].id(), queued_a.id());
    assert_eq!(
        results[0].status,
        BundleStatus::WaitingForService {
            service: service_a.clone()
        }
    );

    // The in-flight bundle and the other service's queue are untouched
    assert_eq!(
        store.get(in_flight_a.id()).await.unwrap().unwrap().status,
        BundleStatus::DeliveryAckPending {
            service: service_a.clone()
        },
        "an in-flight delivery must not be swept"
    );
    assert_eq!(
        store.get(queued_b.id()).await.unwrap().unwrap().status,
        BundleStatus::DeliverPending { service: service_b },
        "another service's queue must not be swept"
    );

    // A second sweep finds nothing left to reset
    assert_eq!(store.reset_service_queue(&service_a).await.unwrap(), 0);
}

/// META-12: Recovery
pub async fn meta_12_recovery(store: Arc<dyn MetadataStorage>) {
    store.start_recovery().await;
    // Should complete without panic or error
}

/// META-13: Remove Unconfirmed
pub async fn meta_13_remove_unconfirmed(store: Arc<dyn MetadataStorage>) {
    let bundle = fixtures::random_bundle();
    assert!(store.insert(&bundle).await.unwrap());

    let sink = super::VecSink::<bundle::Bundle>::new();
    store.remove_unconfirmed(&sink).await.unwrap();
    // Should complete without error
}
