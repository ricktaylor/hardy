use super::*;

/// BLOB-01: Save & Load
pub async fn blob_01_save_and_load(store: Arc<dyn BundleStorage>) {
    let data = fixtures::patterned_payload(1024);
    let name = store.save(data.clone()).await.unwrap();

    let loaded = store.load(&name).await.unwrap();
    let loaded = loaded.expect("load should return Some after save");
    assert_eq!(loaded, data, "loaded bytes should match saved bytes");
}

/// BLOB-02: Delete
pub async fn blob_02_delete(store: Arc<dyn BundleStorage>) {
    let data = fixtures::patterned_payload(512);
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
///
/// `recover()` makes no ordering promise, so all assertions here are
/// membership checks.
pub async fn blob_04_recovery_scan(store: Arc<dyn BundleStorage>) {
    // Generous slack for the timestamp bounds: S3-style backends stamp
    // objects with the server clock, not ours.
    let slack = time::Duration::hours(1);
    let earliest = time::OffsetDateTime::now_utc() - slack;

    let data_a = fixtures::patterned_payload(256);
    let data_b = fixtures::patterned_payload(512);

    let name_a = store.save(data_a).await.unwrap();
    let name_b = store.save(data_b).await.unwrap();

    let sink = super::VecSink::<hardy_bpa::storage::RecoveryResponse>::new();
    store.recover(&sink).await.unwrap();
    let results = sink.into_inner();

    assert!(
        results.len() >= 2,
        "recover should emit entries for saved bundles"
    );
    assert!(
        results.iter().any(|(n, _)| n.as_ref() == name_a.as_ref()),
        "recovery should include first saved bundle"
    );
    assert!(
        results.iter().any(|(n, _)| n.as_ref() == name_b.as_ref()),
        "recovery should include second saved bundle"
    );

    // Each entry should carry a timestamp close to the save time
    let latest = time::OffsetDateTime::now_utc() + slack;
    for (name, ts) in &results {
        assert!(
            *ts >= earliest && *ts <= latest,
            "recovery timestamp for {name} should be close to the save time, got {ts}"
        );
    }
}

/// BLOB-05: Repeatable Load
///
/// Loading is non-destructive: the BPA re-loads on every forwarding retry,
/// so the entry must survive until `delete()`.
pub async fn blob_05_repeatable_load(store: Arc<dyn BundleStorage>) {
    let data = fixtures::patterned_payload(1024);
    let name = store.save(data.clone()).await.unwrap();

    let first = store.load(&name).await.unwrap();
    assert_eq!(
        first.as_ref(),
        Some(&data),
        "first load should return the saved bytes"
    );

    let second = store.load(&name).await.unwrap();
    assert_eq!(
        second.as_ref(),
        Some(&data),
        "load must be repeatable until delete()"
    );
}
