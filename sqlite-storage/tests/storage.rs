use std::sync::Arc;

use hardy_bpa::{bundle::BundleStatus, storage::MetadataStorage};
use hardy_sqlite_storage::SqliteStorage;
use storage_tests::VecSink;

fn make_bundle(dest_service: u64) -> hardy_bpa::bundle::Bundle {
    use hardy_bpv7::{builder::Builder, creation_timestamp::CreationTimestamp, eid::Eid};

    let source: Eid = "ipn:1.0".parse().unwrap();
    let dest: Eid = format!("ipn:2.{dest_service}").parse().unwrap();
    let (raw, _data) = Builder::new(source, dest)
        .with_payload(b"test".to_vec().into())
        .build(CreationTimestamp::now())
        .unwrap();

    // The storage tests below only read `bundle.id()`, so we
    // skip the parse round-trip and use Builder's structural output
    // directly. (Editor-touching tests still need to re-parse for
    // wire-aligned block numbers.)
    hardy_bpa::bundle::Bundle {
        bpv7: raw,
        metadata: hardy_bpa::bundle::BundleMetadata::originated(),
        status: BundleStatus::New,
    }
}

// Database runs in WAL journal mode (set at connection setup; the schema
// copy of the pragma cannot take effect inside the migration transaction).
#[test]
fn test_journal_mode_is_wal() {
    let dir = tempfile::tempdir().unwrap();
    let _storage = SqliteStorage::new(Some(dir.path().to_path_buf()), Some("test.db".into()), true);

    let conn = rusqlite::Connection::open(dir.path().join("test.db")).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
}

// ForwardAckPending round-trips through the status columns, and
// reset_peer_ack_pending flips exactly the matching peer's transfers to
// Waiting.
#[tokio::test]
async fn test_forward_ack_pending_roundtrip_and_reset() {
    let dir = tempfile::tempdir().unwrap();
    let storage = SqliteStorage::new(Some(dir.path().to_path_buf()), Some("test.db".into()), true);

    let mut bundle = make_bundle(1);
    bundle.status = BundleStatus::ForwardAckPending { peer: 7 };
    assert!(storage.insert(&bundle).await.unwrap());

    let got = storage.get(bundle.id()).await.unwrap().unwrap();
    assert_eq!(got.status, BundleStatus::ForwardAckPending { peer: 7 });

    // A different peer's transfer is untouched by the reset
    let mut other = make_bundle(2);
    other.status = BundleStatus::ForwardAckPending { peer: 8 };
    assert!(storage.insert(&other).await.unwrap());

    assert_eq!(storage.reset_peer_ack_pending(7).await.unwrap(), 1);

    let got = storage.get(bundle.id()).await.unwrap().unwrap();
    assert_eq!(got.status, BundleStatus::Waiting);
    let got = storage.get(other.id()).await.unwrap().unwrap();
    assert_eq!(got.status, BundleStatus::ForwardAckPending { peer: 8 });
}

// swap_status applies only when every status column matches the expected
// status, making it the arbiter for outcome-resolution races.
#[tokio::test]
async fn test_swap_status_is_conditional() {
    let dir = tempfile::tempdir().unwrap();
    let storage = SqliteStorage::new(Some(dir.path().to_path_buf()), Some("test.db".into()), true);

    let mut bundle = make_bundle(1);
    bundle.status = BundleStatus::ForwardAckPending { peer: 7 };
    assert!(storage.insert(&bundle).await.unwrap());

    // Wrong peer in the expectation: no swap
    assert!(
        !storage
            .swap_status(
                bundle.id(),
                &BundleStatus::ForwardAckPending { peer: 8 },
                &BundleStatus::Dispatching,
            )
            .await
            .unwrap()
    );

    // Matching expectation: swap applies
    assert!(
        storage
            .swap_status(
                bundle.id(),
                &BundleStatus::ForwardAckPending { peer: 7 },
                &BundleStatus::Dispatching,
            )
            .await
            .unwrap()
    );
    assert_eq!(
        storage.get(bundle.id()).await.unwrap().unwrap().status,
        BundleStatus::Dispatching
    );

    // A duplicate resolution loses
    assert!(
        !storage
            .swap_status(
                bundle.id(),
                &BundleStatus::ForwardAckPending { peer: 7 },
                &BundleStatus::Dispatching,
            )
            .await
            .unwrap()
    );

    // A deleted bundle swaps nothing
    storage.tombstone(bundle.id()).await.unwrap();
    assert!(
        !storage
            .swap_status(
                bundle.id(),
                &BundleStatus::Dispatching,
                &BundleStatus::Waiting,
            )
            .await
            .unwrap()
    );
}

// tombstone_if removes the bundle only when every status column matches
// the expected status: the terminal arbiter for outcome-resolution races.
#[tokio::test]
async fn test_tombstone_if_is_conditional() {
    let dir = tempfile::tempdir().unwrap();
    let storage = SqliteStorage::new(Some(dir.path().to_path_buf()), Some("test.db".into()), true);

    let mut bundle = make_bundle(1);
    bundle.status = BundleStatus::ForwardAckPending { peer: 7 };
    assert!(storage.insert(&bundle).await.unwrap());

    // Wrong peer in the expectation: not tombstoned
    assert!(
        !storage
            .tombstone_if(bundle.id(), &BundleStatus::ForwardAckPending { peer: 8 })
            .await
            .unwrap()
    );
    assert!(storage.get(bundle.id()).await.unwrap().is_some());

    // Matching expectation: tombstoned
    assert!(
        storage
            .tombstone_if(bundle.id(), &BundleStatus::ForwardAckPending { peer: 7 })
            .await
            .unwrap()
    );
    assert!(storage.get(bundle.id()).await.unwrap().is_none());

    // A duplicate resolution loses
    assert!(
        !storage
            .tombstone_if(bundle.id(), &BundleStatus::ForwardAckPending { peer: 7 })
            .await
            .unwrap()
    );
}

// SQL-01: Database is created at the configured path.
#[tokio::test]
async fn test_configuration_custom_db_dir() {
    let dir = tempfile::tempdir().unwrap();
    let _store = SqliteStorage::new(Some(dir.path().to_path_buf()), Some("test.db".into()), true);

    let db_path = dir.path().join("test.db");
    assert!(
        db_path.exists(),
        "database file should be created at configured path"
    );
}
// SQL-04: Concurrent writers and readers do not panic, deadlock, or hit
// SQLITE_BUSY.
//
// The storage runs rusqlite calls inline on the calling task, so genuine
// cross-connection contention needs a multi-thread runtime. A barrier
// releases all tasks at once so the pooled read connections contend with
// the serialised writer instead of running one after another.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_concurrency_no_sqlite_busy() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteStorage::new(
        Some(dir.path().to_path_buf()),
        Some("test.db".into()),
        true,
    ));

    // Create all bundles upfront so we can capture their IDs for verification
    let bundles: Vec<_> = (0..10).map(make_bundle).collect();
    let ids: Vec<_> = bundles.iter().map(|b| b.id().clone()).collect();

    let barrier = Arc::new(tokio::sync::Barrier::new(bundles.len() + ids.len()));

    let mut handles = Vec::new();
    for bundle in bundles {
        let store = store.clone();
        let barrier = barrier.clone();
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            store.insert(&bundle).await.unwrap();
        }));
    }

    // Readers run concurrently with the writers. A None result is fine
    // (the matching insert may not have landed yet); an error is not.
    for id in ids.clone() {
        let store = store.clone();
        let barrier = barrier.clone();
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            store.get(&id).await.unwrap();
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    // Verify all 10 were inserted by reading them back
    for (i, id) in ids.iter().enumerate() {
        let result = store.get(id).await.unwrap();
        assert!(result.is_some(), "bundle {i} should exist");
    }
}

// SQL-05: Corrupt data in the DB does not panic.
//
// `get()` returns an error on corrupt blob data (deserialization failure).
// `confirm_exists()` handles it gracefully by tombstoning the entry.
#[tokio::test]
async fn test_corrupt_data_does_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStorage::new(Some(dir.path().to_path_buf()), Some("test.db".into()), true);

    // Insert a valid bundle
    let bundle = make_bundle(0);
    let id_bytes = serde_json::to_vec(bundle.id()).unwrap();
    assert!(store.insert(&bundle).await.unwrap());

    // Corrupt the bundle blob directly in the DB
    {
        let db_path = dir.path().join("test.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "UPDATE bundles SET bundle = X'DEADBEEF' WHERE bundle_id = ?1",
            [&id_bytes],
        )
        .unwrap();
    }

    // get() returns Err (deserialization failure), not panic
    let result = store.get(bundle.id()).await;
    assert!(result.is_err(), "get() should return Err for corrupt data");

    // confirm_exists() handles it gracefully — tombstones the entry
    store.start_recovery().await;
    let result = store.confirm_exists(bundle.id()).await.unwrap();
    assert!(
        result.is_none(),
        "confirm_exists should return None for corrupt data"
    );

    // Entry should now be tombstoned
    let result = store.get(bundle.id()).await.unwrap();
    assert!(result.is_none(), "tombstoned entry should return None");
}

// SQL-06: Waiting queue is invalidated when bundle status changes.
#[tokio::test]
async fn test_waiting_queue_invalidation() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStorage::new(Some(dir.path().to_path_buf()), Some("test.db".into()), true);

    // Insert a bundle with Waiting status
    let mut bundle = make_bundle(0);
    bundle.status = BundleStatus::Waiting;
    assert!(store.insert(&bundle).await.unwrap());

    // Poll waiting — should return the bundle (populates waiting_queue)
    let sink = VecSink::new();
    store.poll_waiting(&sink).await.unwrap();
    assert_eq!(sink.into_inner().len(), 1, "should poll 1 waiting bundle");

    // Move it out of Waiting
    assert!(
        store
            .swap_status(
                bundle.id(),
                &BundleStatus::Waiting,
                &BundleStatus::Dispatching
            )
            .await
            .unwrap(),
        "the bundle is Waiting, so the swap applies"
    );

    // Poll waiting again — should return nothing
    let sink = VecSink::new();
    store.poll_waiting(&sink).await.unwrap();
    assert_eq!(
        sink.into_inner().len(),
        0,
        "waiting queue should be empty after status change"
    );
}

// DispatchPending survives a store/load round-trip and is polled by
// poll_pending, but a bundle claimed on to Dispatching no longer is.
#[tokio::test]
async fn test_dispatch_pending_round_trip_and_poll() {
    let dir = tempfile::tempdir().unwrap();
    let storage = SqliteStorage::new(Some(dir.path().to_path_buf()), Some("test.db".into()), true);

    let mut bundle = make_bundle(1);
    bundle.status = BundleStatus::DispatchPending;
    assert!(storage.insert(&bundle).await.unwrap());
    assert_eq!(
        storage.get(bundle.id()).await.unwrap().unwrap().status,
        BundleStatus::DispatchPending
    );

    let sink = VecSink::new();
    storage
        .poll_pending(&sink, &BundleStatus::DispatchPending, 16)
        .await
        .unwrap();
    assert_eq!(sink.into_inner().len(), 1, "queued bundle should be polled");

    // The dispatch consumer's claim: once Dispatching, the channel
    // poller's poll_pending must no longer recover it.
    assert!(
        storage
            .swap_status(
                bundle.id(),
                &BundleStatus::DispatchPending,
                &BundleStatus::Dispatching,
            )
            .await
            .unwrap()
    );
    let sink = VecSink::new();
    storage
        .poll_pending(&sink, &BundleStatus::DispatchPending, 16)
        .await
        .unwrap();
    assert_eq!(
        sink.into_inner().len(),
        0,
        "claimed bundle must not be recovered as pending"
    );
}

// DeliverPending and DeliveryAckPending round-trip through the status
// columns; the delivery channel's poll_pending recovers only queued
// (DeliverPending) bundles for exactly the matching service; and
// reset_service_queue re-parks exactly that service's queued bundles to
// WaitingForService, leaving the in-flight (DeliveryAckPending) one
// alone.
#[tokio::test]
async fn test_delivery_statuses_roundtrip_poll_and_reset() {
    let dir = tempfile::tempdir().unwrap();
    let storage = SqliteStorage::new(Some(dir.path().to_path_buf()), Some("test.db".into()), true);

    let service: hardy_bpv7::eid::Eid = "ipn:1.7".parse().unwrap();
    let other_service: hardy_bpv7::eid::Eid = "ipn:1.8".parse().unwrap();

    let mut queued = make_bundle(1);
    queued.status = BundleStatus::DeliverPending {
        service: service.clone(),
    };
    assert!(storage.insert(&queued).await.unwrap());
    assert_eq!(
        storage.get(queued.id()).await.unwrap().unwrap().status,
        BundleStatus::DeliverPending {
            service: service.clone()
        }
    );

    let mut in_flight = make_bundle(2);
    in_flight.status = BundleStatus::DeliveryAckPending {
        service: service.clone(),
    };
    assert!(storage.insert(&in_flight).await.unwrap());
    assert_eq!(
        storage.get(in_flight.id()).await.unwrap().unwrap().status,
        BundleStatus::DeliveryAckPending {
            service: service.clone()
        }
    );

    // Another service's queued bundle: invisible to this service's
    // channel poll and untouched by its sweep.
    let mut other = make_bundle(3);
    other.status = BundleStatus::DeliverPending {
        service: other_service.clone(),
    };
    assert!(storage.insert(&other).await.unwrap());

    // The channel poller recovers exactly the one queued bundle for the
    // service — never the in-flight one, never another service's.
    let sink = VecSink::new();
    storage
        .poll_pending(
            &sink,
            &BundleStatus::DeliverPending {
                service: service.clone(),
            },
            16,
        )
        .await
        .unwrap();
    let polled = sink.into_inner();
    assert_eq!(polled.len(), 1, "exactly the queued bundle is recoverable");
    assert_eq!(polled[0].id(), queued.id());

    // Unregistration sweep: queued → WaitingForService (same service
    // key), in-flight and other-service bundles untouched.
    assert_eq!(storage.reset_service_queue(&service).await.unwrap(), 1);
    assert_eq!(
        storage.get(queued.id()).await.unwrap().unwrap().status,
        BundleStatus::WaitingForService {
            service: service.clone()
        }
    );
    assert_eq!(
        storage.get(in_flight.id()).await.unwrap().unwrap().status,
        BundleStatus::DeliveryAckPending { service }
    );
    assert_eq!(
        storage.get(other.id()).await.unwrap().unwrap().status,
        BundleStatus::DeliverPending {
            service: other_service
        }
    );
}
