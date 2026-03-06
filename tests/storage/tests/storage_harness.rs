use storage_tests::*;

// MetadataStorage backends
storage_meta_tests!(memory, storage_tests::memory_meta_setup);
storage_meta_tests!(sqlite, storage_tests::sqlite_meta_setup);

// BundleStorage backends
storage_blob_tests!(memory_blob, storage_tests::memory_blob_setup);
storage_blob_tests!(localdisk, storage_tests::localdisk_blob_setup);

// Recovery protocol tests — only applicable to persistent backends
mod sqlite_recovery {
    #[tokio::test]
    async fn meta_05_confirm_exists() {
        let (_dir, store) = storage_tests::sqlite_meta_setup();
        storage_tests::metadata_suite::meta_05_confirm_exists(store).await;
    }
}
