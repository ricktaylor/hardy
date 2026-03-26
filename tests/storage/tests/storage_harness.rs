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

// PostgreSQL metadata storage (requires --features postgres + a running postgres instance)
// Default connection: postgresql://hardy:hardy@localhost:5432
// Override: TEST_POSTGRES_URL=postgresql://user:pass@host:port
#[cfg(feature = "postgres")]
storage_meta_tests_async!(postgres, storage_tests::postgres_meta_setup);

#[cfg(feature = "postgres")]
mod postgres_recovery {
    #[tokio::test]
    async fn meta_05_confirm_exists() {
        let (_guard, store) = storage_tests::postgres_meta_setup().await;
        storage_tests::metadata_suite::meta_05_confirm_exists(store).await;
    }
}

// S3-compatible bundle storage (requires --features s3 + a running MinIO/S3 instance)
// Default endpoint: http://localhost:9000, bucket: hardy-test
// Override endpoint: TEST_S3_ENDPOINT=http://...
// Override bucket: TEST_S3_BUCKET=...
// Credentials: AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY
#[cfg(feature = "s3")]
storage_blob_tests_async!(s3, storage_tests::s3_blob_setup);
