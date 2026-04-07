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

    let got = store.get(&bundle.bundle.id).await.unwrap();
    let got = got.expect("get should return Some after insert");

    assert_eq!(got.bundle.id, bundle.bundle.id);
    assert_eq!(got.metadata.status, bundle.metadata.status);
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

/// META-03: Update (Replace)
pub async fn meta_03_update_replace(store: Arc<dyn MetadataStorage>) {
    let mut bundle = fixtures::random_bundle();
    bundle.metadata.status = BundleStatus::Waiting;
    assert!(store.insert(&bundle).await.unwrap());

    bundle.metadata.status = BundleStatus::Dispatching;
    store.replace(&bundle).await.unwrap();

    let got = store.get(&bundle.bundle.id).await.unwrap().unwrap();
    assert_eq!(got.metadata.status, BundleStatus::Dispatching);
}

/// META-04: Tombstone
pub async fn meta_04_tombstone(store: Arc<dyn MetadataStorage>) {
    let bundle = fixtures::random_bundle();
    assert!(store.insert(&bundle).await.unwrap());

    store.tombstone(&bundle.bundle.id).await.unwrap();

    let got = store.get(&bundle.bundle.id).await.unwrap();
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
    let bundle = fixtures::random_bundle();
    let missing_id = fixtures::random_bundle().bundle.id;

    // Simulate a previous session: bundle already exists in the store
    assert!(store.insert(&bundle).await.unwrap());

    // Start recovery — marks all existing entries as unconfirmed
    store.start_recovery().await;

    // Confirm the bundle we know about
    let exists = store.confirm_exists(&bundle.bundle.id).await.unwrap();
    assert!(
        exists.is_some(),
        "confirm_exists should return Some for existing bundle"
    );

    // A bundle ID that was never inserted should return None
    let missing = store.confirm_exists(&missing_id).await.unwrap();
    assert!(
        missing.is_none(),
        "confirm_exists should return None for missing bundle"
    );

    // The confirmed bundle should survive remove_unconfirmed
    let (tx, rx) = flume::unbounded();
    store.remove_unconfirmed(tx).await.unwrap();
    let removed: Vec<_> = rx.try_iter().collect();
    assert!(removed.is_empty(), "confirmed bundle should not be removed");

    // The confirmed bundle should still be retrievable
    let got = store.get(&bundle.bundle.id).await.unwrap();
    assert!(
        got.is_some(),
        "confirmed bundle should still exist after remove_unconfirmed"
    );
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

    let (tx, rx) = flume::unbounded();
    store.poll_waiting(tx).await.unwrap();
    let results: Vec<_> = rx.try_iter().collect();

    assert_eq!(results.len(), 2, "should return both Waiting bundles");
    assert_eq!(
        results[0].bundle.id, bundle_a.bundle.id,
        "first should be earlier bundle"
    );
    assert_eq!(
        results[1].bundle.id, bundle_b.bundle.id,
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

    assert!(store.insert(&bundle_a).await.unwrap());
    assert!(store.insert(&bundle_b).await.unwrap());
    assert!(store.insert(&bundle_c).await.unwrap());

    // Full poll: should return B then A, excluding C (New status)
    let (tx, rx) = flume::unbounded();
    store.poll_expiry(tx, 10).await.unwrap();
    let results: Vec<_> = rx.try_iter().collect();

    assert_eq!(results.len(), 2, "New-status bundle should be excluded");
    assert_eq!(
        results[0].bundle.id, bundle_b.bundle.id,
        "first should be the bundle with earlier expiry"
    );
    assert_eq!(
        results[1].bundle.id, bundle_a.bundle.id,
        "second should be the bundle with later expiry"
    );

    // Limit test: limit=1 should return only the earliest-expiry bundle
    let (tx, rx) = flume::unbounded();
    store.poll_expiry(tx, 1).await.unwrap();
    let results: Vec<_> = rx.try_iter().collect();

    assert_eq!(results.len(), 1, "limit=1 should return exactly 1 bundle");
    assert_eq!(results[0].bundle.id, bundle_b.bundle.id);
}

/// META-08: Poll Pending (FIFO & Limit)
pub async fn meta_08_poll_pending_limit(store: Arc<dyn MetadataStorage>) {
    let now = time::OffsetDateTime::now_utc();
    let earlier = now - time::Duration::seconds(100);
    let later = now + time::Duration::seconds(100);

    let status = BundleStatus::ForwardPending {
        peer: 42,
        queue: Some(0),
    };

    let bundle_a = fixtures::bundle_with_status(status.clone(), earlier);
    let bundle_b = fixtures::bundle_with_status(status.clone(), later);

    assert!(store.insert(&bundle_a).await.unwrap());
    assert!(store.insert(&bundle_b).await.unwrap());

    // limit=1: should return only the first (earlier) bundle
    let (tx, rx) = flume::unbounded();
    store.poll_pending(tx, &status, 1).await.unwrap();
    let results: Vec<_> = rx.try_iter().collect();

    assert_eq!(results.len(), 1, "limit=1 should return exactly 1 bundle");
    assert_eq!(
        results[0].bundle.id, bundle_a.bundle.id,
        "should be FIFO (earlier first)"
    );

    // limit=2: should return both in FIFO order
    let (tx, rx) = flume::unbounded();
    store.poll_pending(tx, &status, 2).await.unwrap();
    let results: Vec<_> = rx.try_iter().collect();

    assert_eq!(results.len(), 2, "limit=2 should return both bundles");
    assert_eq!(
        results[0].bundle.id, bundle_a.bundle.id,
        "first should be earlier"
    );
    assert_eq!(
        results[1].bundle.id, bundle_b.bundle.id,
        "second should be later"
    );
}

/// META-09: Poll Pending (Exact Match)
pub async fn meta_09_poll_pending_exact_match(store: Arc<dyn MetadataStorage>) {
    let now = time::OffsetDateTime::now_utc();

    let status_a = BundleStatus::ForwardPending {
        peer: 1,
        queue: Some(0),
    };
    let status_b = BundleStatus::ForwardPending {
        peer: 2,
        queue: Some(0),
    };
    let status_c = BundleStatus::ForwardPending {
        peer: 1,
        queue: Some(1),
    };

    let bundle_a = fixtures::bundle_with_status(status_a.clone(), now);
    let bundle_b = fixtures::bundle_with_status(status_b, now);
    let bundle_c = fixtures::bundle_with_status(status_c, now);

    assert!(store.insert(&bundle_a).await.unwrap());
    assert!(store.insert(&bundle_b).await.unwrap());
    assert!(store.insert(&bundle_c).await.unwrap());

    let (tx, rx) = flume::unbounded();
    store.poll_pending(tx, &status_a, 10).await.unwrap();
    let results: Vec<_> = rx.try_iter().collect();

    assert_eq!(
        results.len(),
        1,
        "only exact-matching status should be returned"
    );
    assert_eq!(results[0].bundle.id, bundle_a.bundle.id);
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

    let (tx, rx) = flume::unbounded();
    store.poll_adu_fragments(tx, &status).await.unwrap();
    let results: Vec<_> = rx.try_iter().collect();

    assert_eq!(results.len(), 2, "should return both fragments");
    assert_eq!(
        results[0].bundle.id.fragment_info.as_ref().unwrap().offset,
        0,
        "first should be offset=0"
    );
    assert_eq!(
        results[1].bundle.id.fragment_info.as_ref().unwrap().offset,
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
    let (tx, rx) = flume::unbounded();
    store
        .poll_service_waiting(service_a.clone(), tx)
        .await
        .unwrap();
    let results: Vec<_> = rx.try_iter().collect();

    assert_eq!(results.len(), 2, "should return both bundles for service_a");
    assert_eq!(
        results[0].bundle.id, bundle_a2.bundle.id,
        "first should be earlier bundle"
    );
    assert_eq!(
        results[0].metadata.status, status_a,
        "returned bundle should have correct WaitingForService status"
    );
    assert_eq!(
        results[1].bundle.id, bundle_a1.bundle.id,
        "second should be later bundle"
    );

    // Poll for service_b — should return only the one matching bundle
    let (tx, rx) = flume::unbounded();
    store.poll_service_waiting(service_b, tx).await.unwrap();
    let results: Vec<_> = rx.try_iter().collect();

    assert_eq!(results.len(), 1, "should return only bundle for service_b");
    assert_eq!(results[0].bundle.id, bundle_b1.bundle.id);
}

// ---------------------------------------------------------------------------
// Suite C: State Transitions & Bulk Ops
// ---------------------------------------------------------------------------

/// META-11: Reset Peer Queue
pub async fn meta_11_reset_peer_queue(store: Arc<dyn MetadataStorage>) {
    let now = time::OffsetDateTime::now_utc();

    let status_100 = BundleStatus::ForwardPending {
        peer: 100,
        queue: Some(0),
    };
    let status_200 = BundleStatus::ForwardPending {
        peer: 200,
        queue: Some(0),
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

    let got_a = store.get(&bundle_a.bundle.id).await.unwrap().unwrap();
    assert_eq!(
        got_a.metadata.status,
        BundleStatus::Waiting,
        "peer 100 bundle should become Waiting"
    );

    let got_b = store.get(&bundle_b.bundle.id).await.unwrap().unwrap();
    assert_eq!(
        got_b.metadata.status, status_200,
        "peer 200 bundle should remain ForwardPending"
    );
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

    let (tx, _rx) = flume::unbounded();
    store.remove_unconfirmed(tx).await.unwrap();
    // Should complete without error
}
