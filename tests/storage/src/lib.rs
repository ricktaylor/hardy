use std::sync::Arc;

use bytes::Bytes;
use hardy_bpa::bundle;
use hardy_bpa::metadata::{BundleMetadata, BundleStatus};
use hardy_bpa::storage::{BundleStorage, MetadataStorage};
use hardy_bpv7::creation_timestamp::CreationTimestamp;

pub mod bundle_suite;
pub mod fixtures;
pub mod metadata_suite;

// ---------------------------------------------------------------------------
// Backend setup functions
// ---------------------------------------------------------------------------

pub fn memory_meta_setup() -> ((), Arc<dyn MetadataStorage>) {
    (
        (),
        Arc::new(hardy_bpa::storage::metadata_mem::MetadataMemStorage::new(
            &Default::default(),
        )),
    )
}

pub fn sqlite_meta_setup() -> (tempfile::TempDir, Arc<dyn MetadataStorage>) {
    let dir = tempfile::tempdir().unwrap();
    let config = hardy_sqlite_storage::Config {
        db_dir: dir.path().into(),
        ..Default::default()
    };
    let store = hardy_sqlite_storage::new(&config, true);
    (dir, store)
}

pub fn memory_blob_setup() -> ((), Arc<dyn BundleStorage>) {
    (
        (),
        Arc::new(hardy_bpa::storage::bundle_mem::BundleMemStorage::new(
            &Default::default(),
        )),
    )
}

pub fn localdisk_blob_setup() -> (tempfile::TempDir, Arc<dyn BundleStorage>) {
    let dir = tempfile::tempdir().unwrap();
    let config = hardy_localdisk_storage::Config {
        store_dir: dir.path().into(),
        ..Default::default()
    };
    let store = hardy_localdisk_storage::new(&config, false);
    (dir, store)
}

// ---------------------------------------------------------------------------
// PostgreSQL backend setup (feature = "postgres")
// ---------------------------------------------------------------------------
//
// Each call creates a fresh database with a random name so tests are fully
// isolated and can run in parallel. The returned guard drops the database
// when the test completes (even on panic).

#[cfg(feature = "postgres")]
pub struct PostgresTestGuard {
    maintenance_url: String,
    db_name: String,
}

#[cfg(feature = "postgres")]
impl Drop for PostgresTestGuard {
    fn drop(&mut self) {
        let url = self.maintenance_url.clone();
        let db_name = self.db_name.clone();
        // Spawn a dedicated OS thread + runtime so we can run async cleanup
        // from a synchronous Drop context (we may be inside a tokio executor).
        let _ = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("cleanup runtime")
                .block_on(async move {
                    use sqlx::Connection as _;
                    if let Ok(mut conn) = sqlx::postgres::PgConnection::connect(&url).await {
                        let _ =
                            sqlx::query(&format!("DROP DATABASE IF EXISTS \"{db_name}\" (FORCE)"))
                                .execute(&mut conn)
                                .await;
                        let _ = conn.close().await;
                    }
                });
        })
        .join();
    }
}

/// Creates a fresh PostgreSQL database for one test.
///
/// Reads `TEST_POSTGRES_URL` (default: `postgresql://hardy:hardy@localhost:5432`)
/// — this should be the base URL **without** a database name. A unique database
/// is created for each call and dropped when the returned guard is dropped.
#[cfg(feature = "postgres")]
pub async fn postgres_meta_setup() -> (PostgresTestGuard, Arc<dyn MetadataStorage>) {
    use sqlx::postgres::PgConnectOptions;

    let base_url = std::env::var("TEST_POSTGRES_URL")
        .unwrap_or_else(|_| "postgresql://hardy:hardy@localhost:5432".to_string());

    let base_opts: PgConnectOptions = base_url.parse().expect("invalid TEST_POSTGRES_URL");

    let db_name = format!("hardy_test_{}", uuid::Uuid::new_v4().simple());

    // Create the test database via a single connection (not a pool) to avoid
    // exhausting connection slots when many tests run in parallel.
    {
        use sqlx::Connection as _;
        let mut conn =
            sqlx::postgres::PgConnection::connect_with(&base_opts.clone().database("postgres"))
                .await
                .unwrap_or_else(|e| panic!("connect to postgres ({base_url}/postgres): {e}"));
        sqlx::query(&format!("CREATE DATABASE \"{db_name}\""))
            .execute(&mut conn)
            .await
            .expect("create test database");
        conn.close().await.expect("close maintenance connection");
    }

    let config = hardy_postgres_storage::Config {
        database_url: format!("{base_url}/{db_name}"),
        max_connections: 5,
        min_connections: 1,
        connect_timeout_secs: 10,
        ..Default::default()
    };
    let store = hardy_postgres_storage::new(&config, true)
        .await
        .unwrap_or_else(|e| panic!("open test database ({base_url}/{db_name}): {e}"));

    (
        PostgresTestGuard {
            maintenance_url: format!("{base_url}/postgres"),
            db_name,
        },
        store,
    )
}

// ---------------------------------------------------------------------------
// S3-compatible backend setup (feature = "s3")
// ---------------------------------------------------------------------------
//
// Each call uses a unique key prefix so tests are isolated within the bucket
// and can run in parallel. Credentials are read from the standard AWS env vars.

/// Creates an S3 bundle storage backed by a unique key prefix.
///
/// Reads:
/// - `TEST_S3_ENDPOINT` (default: `http://localhost:9000`) — MinIO or any
///   S3-compatible endpoint. Leave unset for real AWS.
/// - `TEST_S3_BUCKET` (default: `hardy-test`) — bucket name.
/// - `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` — credentials.
#[cfg(feature = "s3")]
pub async fn s3_blob_setup() -> ((), Arc<dyn BundleStorage>) {
    let endpoint = std::env::var("TEST_S3_ENDPOINT").ok();
    let bucket = std::env::var("TEST_S3_BUCKET").unwrap_or_else(|_| "hardy-test".to_string());
    let prefix = format!("test-{}", uuid::Uuid::new_v4().simple());

    let region = std::env::var("AWS_DEFAULT_REGION")
        .or_else(|_| std::env::var("AWS_REGION"))
        .ok()
        .or_else(|| endpoint.as_ref().map(|_| "us-east-1".to_string()));

    let config = hardy_s3_storage::Config {
        bucket,
        prefix,
        region,
        endpoint_url: endpoint,
        force_path_style: true,
        ..Default::default()
    };
    let store = hardy_s3_storage::new(&config).await.unwrap_or_else(|e| {
        let endpoint = config.endpoint_url.as_deref().unwrap_or("(AWS default)");
        panic!(
            "connect to S3/MinIO (bucket={}, endpoint={endpoint}): {e}",
            config.bucket
        )
    });
    ((), store)
}

// ---------------------------------------------------------------------------
// Test generation macros
// ---------------------------------------------------------------------------

#[macro_export]
macro_rules! storage_meta_tests {
    ($mod_name:ident, $setup:path) => {
        mod $mod_name {
            use super::*;

            macro_rules! meta_test {
                ($name:ident) => {
                    #[tokio::test]
                    async fn $name() {
                        let (_cleanup, store) = $setup();
                        storage_tests::metadata_suite::$name(store).await;
                    }
                };
            }

            meta_test!(meta_01_insert_and_get);
            meta_test!(meta_02_duplicate_insert);
            meta_test!(meta_03_update_replace);
            meta_test!(meta_04_tombstone);
            meta_test!(meta_06_poll_waiting_fifo);
            meta_test!(meta_07_poll_expiry);
            meta_test!(meta_08_poll_pending_limit);
            meta_test!(meta_09_poll_pending_exact_match);
            meta_test!(meta_10_poll_adu_fragments);
            meta_test!(meta_11_reset_peer_queue);
            meta_test!(meta_14_poll_service_waiting);
            meta_test!(meta_12_recovery);
            meta_test!(meta_13_remove_unconfirmed);
        }
    };
}

#[macro_export]
macro_rules! storage_blob_tests {
    ($mod_name:ident, $setup:path) => {
        mod $mod_name {
            use super::*;

            macro_rules! blob_test {
                ($name:ident) => {
                    #[tokio::test]
                    async fn $name() {
                        let (_cleanup, store) = $setup();
                        storage_tests::bundle_suite::$name(store).await;
                    }
                };
            }

            blob_test!(blob_01_save_and_load);
            blob_test!(blob_02_delete);
            blob_test!(blob_03_missing_load);
            blob_test!(blob_04_recovery_scan);
        }
    };
}

/// Like [`storage_meta_tests!`] but for async setup functions (e.g. postgres).
#[macro_export]
macro_rules! storage_meta_tests_async {
    ($mod_name:ident, $setup:path) => {
        mod $mod_name {
            use super::*;

            macro_rules! meta_test {
                ($name:ident) => {
                    #[tokio::test]
                    async fn $name() {
                        let (_cleanup, store) = $setup().await;
                        storage_tests::metadata_suite::$name(store).await;
                    }
                };
            }

            meta_test!(meta_01_insert_and_get);
            meta_test!(meta_02_duplicate_insert);
            meta_test!(meta_03_update_replace);
            meta_test!(meta_04_tombstone);
            meta_test!(meta_06_poll_waiting_fifo);
            meta_test!(meta_07_poll_expiry);
            meta_test!(meta_08_poll_pending_limit);
            meta_test!(meta_09_poll_pending_exact_match);
            meta_test!(meta_10_poll_adu_fragments);
            meta_test!(meta_11_reset_peer_queue);
            meta_test!(meta_14_poll_service_waiting);
            meta_test!(meta_12_recovery);
            meta_test!(meta_13_remove_unconfirmed);
        }
    };
}

/// Like [`storage_blob_tests!`] but for async setup functions (e.g. s3).
#[macro_export]
macro_rules! storage_blob_tests_async {
    ($mod_name:ident, $setup:path) => {
        mod $mod_name {
            use super::*;

            macro_rules! blob_test {
                ($name:ident) => {
                    #[tokio::test]
                    async fn $name() {
                        let (_cleanup, store) = $setup().await;
                        storage_tests::bundle_suite::$name(store).await;
                    }
                };
            }

            blob_test!(blob_01_save_and_load);
            blob_test!(blob_02_delete);
            blob_test!(blob_03_missing_load);
            blob_test!(blob_04_recovery_scan);
        }
    };
}
