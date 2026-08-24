use std::sync::Arc;

use hardy_bpa::{bundle::BundleStatus, storage::MetadataStorage};
use storage_tests::VecSink;

use hardy_sqlite_storage::SqliteStorage;

fn make_bundle(dest_service: u64) -> hardy_bpa::bundle::Bundle {
    use hardy_bpv7::{builder::Builder, creation_timestamp::CreationTimestamp, eid::Eid};

    let source: Eid = "ipn:1.0".parse().unwrap();
    let dest: Eid = format!("ipn:2.{dest_service}").parse().unwrap();
    let (_bundle, data) = Builder::new(source, dest)
        .with_payload(b"test".to_vec().into())
        .build(CreationTimestamp::now())
        .unwrap();

    let parsed =
        hardy_bpv7::bundle::ParsedBundle::parse(&data, hardy_bpv7::bpsec::no_keys).unwrap();

    hardy_bpa::bundle::Bundle {
        bundle: parsed.bundle,
        metadata: hardy_bpa::bundle::BundleMetadata::default(),
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
    bundle.metadata.status = BundleStatus::ForwardAckPending { peer: 7 };
    assert!(storage.insert(&bundle).await.unwrap());

    let got = storage.get(&bundle.bundle.id).await.unwrap().unwrap();
    assert_eq!(
        got.metadata.status,
        BundleStatus::ForwardAckPending { peer: 7 }
    );

    // A different peer's transfer is untouched by the reset
    let mut other = make_bundle(2);
    other.metadata.status = BundleStatus::ForwardAckPending { peer: 8 };
    assert!(storage.insert(&other).await.unwrap());

    assert_eq!(storage.reset_peer_ack_pending(7).await.unwrap(), 1);

    let got = storage.get(&bundle.bundle.id).await.unwrap().unwrap();
    assert_eq!(got.metadata.status, BundleStatus::Waiting);
    let got = storage.get(&other.bundle.id).await.unwrap().unwrap();
    assert_eq!(
        got.metadata.status,
        BundleStatus::ForwardAckPending { peer: 8 }
    );
}

// swap_status applies only when every status column matches the expected
// status, making it the arbiter for outcome-resolution races.
#[tokio::test]
async fn test_swap_status_is_conditional() {
    let dir = tempfile::tempdir().unwrap();
    let storage = SqliteStorage::new(Some(dir.path().to_path_buf()), Some("test.db".into()), true);

    let mut bundle = make_bundle(1);
    bundle.metadata.status = BundleStatus::ForwardAckPending { peer: 7 };
    assert!(storage.insert(&bundle).await.unwrap());

    // Wrong peer in the expectation: no swap
    assert!(
        !storage
            .swap_status(
                &bundle.bundle.id,
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
                &bundle.bundle.id,
                &BundleStatus::ForwardAckPending { peer: 7 },
                &BundleStatus::Dispatching,
            )
            .await
            .unwrap()
    );
    assert_eq!(
        storage
            .get(&bundle.bundle.id)
            .await
            .unwrap()
            .unwrap()
            .metadata
            .status,
        BundleStatus::Dispatching
    );

    // A duplicate resolution loses
    assert!(
        !storage
            .swap_status(
                &bundle.bundle.id,
                &BundleStatus::ForwardAckPending { peer: 7 },
                &BundleStatus::Dispatching,
            )
            .await
            .unwrap()
    );

    // A deleted bundle swaps nothing
    storage.tombstone(&bundle.bundle.id).await.unwrap();
    assert!(
        !storage
            .swap_status(
                &bundle.bundle.id,
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
    bundle.metadata.status = BundleStatus::ForwardAckPending { peer: 7 };
    assert!(storage.insert(&bundle).await.unwrap());

    // Wrong peer in the expectation: not tombstoned
    assert!(
        !storage
            .tombstone_if(
                &bundle.bundle.id,
                &BundleStatus::ForwardAckPending { peer: 8 }
            )
            .await
            .unwrap()
    );
    assert!(storage.get(&bundle.bundle.id).await.unwrap().is_some());

    // Matching expectation: tombstoned
    assert!(
        storage
            .tombstone_if(
                &bundle.bundle.id,
                &BundleStatus::ForwardAckPending { peer: 7 }
            )
            .await
            .unwrap()
    );
    assert!(storage.get(&bundle.bundle.id).await.unwrap().is_none());

    // A duplicate resolution loses
    assert!(
        !storage
            .tombstone_if(
                &bundle.bundle.id,
                &BundleStatus::ForwardAckPending { peer: 7 }
            )
            .await
            .unwrap()
    );
}

// A status write against a tombstone quietly loses: the tombstone's
// status columns stay NULL rather than being written back. Checked
// against the raw row, because get() shields readers by filtering on
// live bundles.
#[tokio::test]
async fn test_update_status_does_not_resurrect_tombstone() {
    let dir = tempfile::tempdir().unwrap();
    let storage = SqliteStorage::new(Some(dir.path().to_path_buf()), Some("test.db".into()), true);

    let mut bundle = make_bundle(1);
    bundle.metadata.status = BundleStatus::ForwardAckPending { peer: 7 };
    assert!(storage.insert(&bundle).await.unwrap());
    storage.tombstone(&bundle.bundle.id).await.unwrap();

    bundle.metadata.status = BundleStatus::Waiting;
    storage.update_status(&bundle).await.unwrap();

    assert!(storage.get(&bundle.bundle.id).await.unwrap().is_none());

    let conn = rusqlite::Connection::open(dir.path().join("test.db")).unwrap();
    let (bundle_col, status_code): (Option<Vec<u8>>, Option<i64>) = conn
        .query_row(
            "SELECT bundle, status_code FROM bundles WHERE bundle_id = ?1",
            [serde_json::to_vec(&bundle.bundle.id).unwrap()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(bundle_col.is_none(), "tombstone must keep bundle NULL");
    assert!(status_code.is_none(), "tombstone must keep status NULL");
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

// SQL-04: Concurrent writers and readers complete without SQLITE_BUSY,
// panics, or deadlocks.
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
    let ids: Vec<_> = bundles.iter().map(|b| b.bundle.id.clone()).collect();

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
    let id_bytes = serde_json::to_vec(&bundle.bundle.id).unwrap();
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
    let result = store.get(&bundle.bundle.id).await;
    assert!(result.is_err(), "get() should return Err for corrupt data");

    // confirm_exists() handles it gracefully — tombstones the entry
    store.start_recovery().await;
    let result = store.confirm_exists(&bundle.bundle.id).await.unwrap();
    assert!(
        result.is_none(),
        "confirm_exists should return None for corrupt data"
    );

    // Entry should now be tombstoned
    let result = store.get(&bundle.bundle.id).await.unwrap();
    assert!(result.is_none(), "tombstoned entry should return None");
}

// SQL-06: Waiting queue is invalidated when bundle status changes.
#[tokio::test]
async fn test_waiting_queue_invalidation() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStorage::new(Some(dir.path().to_path_buf()), Some("test.db".into()), true);

    // Insert a bundle with Waiting status
    let mut bundle = make_bundle(0);
    bundle.metadata.status = BundleStatus::Waiting;
    assert!(store.insert(&bundle).await.unwrap());

    // Poll waiting — should return the bundle (populates waiting_queue)
    let sink = VecSink::new();
    store.poll_waiting(&sink).await.unwrap();
    assert_eq!(sink.into_inner().len(), 1, "should poll 1 waiting bundle");

    // Update status to Dispatching
    bundle.metadata.status = BundleStatus::Dispatching;
    store.replace(&bundle).await.unwrap();

    // Poll waiting again — should return nothing
    let sink = VecSink::new();
    store.poll_waiting(&sink).await.unwrap();
    assert_eq!(
        sink.into_inner().len(),
        0,
        "waiting queue should be empty after status change"
    );
}
