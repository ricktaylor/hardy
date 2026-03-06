use super::*;
use fixtures;

/// BLOB-01: Save & Load
pub async fn blob_01_save_and_load(store: Arc<dyn BundleStorage>) {
    let data = fixtures::random_payload(1024);
    let name = store.save(data.clone()).await.unwrap();

    let loaded = store.load(&name).await.unwrap();
    let loaded = loaded.expect("load should return Some after save");
    assert_eq!(loaded, data, "loaded bytes should match saved bytes");
}

/// BLOB-02: Delete
pub async fn blob_02_delete(store: Arc<dyn BundleStorage>) {
    let data = fixtures::random_payload(512);
    let name = store.save(data).await.unwrap();

    store.delete(&name).await.unwrap();

    let loaded = store.load(&name).await.unwrap();
    assert!(loaded.is_none(), "load after delete should return None");
}

/// BLOB-03: Missing Load
pub async fn blob_03_missing_load(store: Arc<dyn BundleStorage>) {
    let result = store.load("non-existent-storage-name").await;
    assert!(result.is_ok(), "missing load should not error");
    assert!(result.unwrap().is_none(), "missing load should return None");
}

/// BLOB-04: Recovery Scan
pub async fn blob_04_recovery_scan(store: Arc<dyn BundleStorage>) {
    let data_a = fixtures::random_payload(256);
    let data_b = fixtures::random_payload(512);

    let name_a = store.save(data_a).await.unwrap();
    let name_b = store.save(data_b).await.unwrap();

    let (tx, rx) = flume::unbounded();
    store.recover(tx).await.unwrap();
    let mut results: Vec<_> = rx.try_iter().collect();

    assert!(
        results.len() >= 2,
        "recover should emit entries for saved bundles"
    );

    // Sort by name for deterministic comparison
    results.sort_by(|a, b| a.0.cmp(&b.0));

    let names: Vec<&str> = results.iter().map(|(n, _)| n.as_ref()).collect();
    assert!(
        names.contains(&name_a.as_ref()),
        "recovery should include first saved bundle"
    );
    assert!(
        names.contains(&name_b.as_ref()),
        "recovery should include second saved bundle"
    );

    // Each entry should have a valid timestamp
    for (_, ts) in &results {
        assert!(
            *ts > time::OffsetDateTime::UNIX_EPOCH,
            "recovery timestamp should be valid"
        );
    }
}
