use std::sync::Arc;

use bytes::Bytes;
use hardy_bpa::{
    bundle::{self, BundleMetadata, BundleStatus},
    storage::{BundleStorage, MetadataStorage},
    stream::{SendError, Sender},
};
use hardy_bpv7::creation_timestamp::CreationTimestamp;

pub mod bundle_suite;
pub mod fixtures;
pub mod metadata_suite;

// ---------------------------------------------------------------------------
// Test sink: collects items into a Vec for assertions.
// ---------------------------------------------------------------------------

/// A `Sender<T>` implementation that collects items into a `Vec` for
/// the test suites to assert against.
pub struct VecSink<T>(std::sync::Mutex<Vec<T>>);

impl<T> VecSink<T> {
    pub fn new() -> Self {
        Self(std::sync::Mutex::new(Vec::new()))
    }

    pub fn into_inner(self) -> Vec<T> {
        self.0.into_inner().unwrap()
    }
}

impl<T> Default for VecSink<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[hardy_bpa::async_trait]
impl<T: Send + Sync + 'static> Sender<T> for VecSink<T> {
    async fn send(&self, item: T) -> Result<(), SendError<T>> {
        self.0.lock().unwrap().push(item);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Backend setup functions
// ---------------------------------------------------------------------------
//
// All setup functions are async (even the ones with nothing to await) so the
// test-generation macros below can treat every backend uniformly.

pub async fn memory_meta_setup() -> ((), Arc<dyn MetadataStorage>) {
    (
        (),
        Arc::new(hardy_bpa::storage::MetadataMemStorage::new(None)),
    )
}

pub async fn sqlite_meta_setup() -> (tempfile::TempDir, Arc<dyn MetadataStorage>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(hardy_sqlite_storage::SqliteStorage::new(
        Some(dir.path().into()),
        None,
        true,
    ));
    (dir, store)
}

pub async fn memory_blob_setup() -> ((), Arc<dyn BundleStorage>) {
    (
        (),
        Arc::new(hardy_bpa::storage::BundleMemStorage::new(None, None)),
    )
}

pub async fn localdisk_blob_setup() -> (tempfile::TempDir, Arc<dyn BundleStorage>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(hardy_localdisk_storage::LocalDiskStorage::new(
        Some(dir.path().into()),
        None,
        false,
    ));
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
        // Failures are reported to stderr but never panic: a leaked
        // hardy_test_* database should be visible, not fatal.
        let joined = std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!(
                        "warning: failed to build cleanup runtime for test database {db_name}: {e}"
                    );
                    return;
                }
            };
            rt.block_on(async move {
                use sqlx::Connection as _;
                match sqlx::postgres::PgConnection::connect(&url).await {
                    Ok(mut conn) => {
                        if let Err(e) = sqlx::query(sqlx::AssertSqlSafe(format!(
                            "DROP DATABASE IF EXISTS \"{db_name}\" (FORCE)"
                        )))
                        .execute(&mut conn)
                        .await
                        {
                            eprintln!("warning: failed to drop test database {db_name}: {e}");
                        }
                        let _ = conn.close().await;
                    }
                    Err(e) => {
                        eprintln!(
                            "warning: failed to connect to drop test database {db_name}: {e}"
                        );
                    }
                }
            });
        })
        .join();
        if joined.is_err() {
            eprintln!(
                "warning: cleanup thread for test database {} panicked",
                self.db_name
            );
        }
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
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE DATABASE \"{db_name}\""
        )))
        .execute(&mut conn)
        .await
        .expect("create test database");
        conn.close().await.expect("close maintenance connection");
    }

    let store = Arc::new(
        hardy_postgres_storage::PostgresStorage::builder()
            .database_url(format!("{base_url}/{db_name}"))
            .max_connections(core::num::NonZeroU32::new(5).unwrap())
            .min_connections(1)
            .connect_timeout(std::time::Duration::from_secs(10))
            .build(true)
            .await
            .unwrap_or_else(|e| panic!("open test database ({base_url}/{db_name}): {e}")),
    );

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
// The returned guard removes the prefix's objects when the test completes.

#[cfg(feature = "s3")]
pub struct S3TestGuard {
    store: Arc<dyn BundleStorage>,
    prefix: String,
}

#[cfg(feature = "s3")]
impl Drop for S3TestGuard {
    fn drop(&mut self) {
        let store = self.store.clone();
        let prefix = self.prefix.clone();
        // Best-effort removal of every object under the test prefix, reusing
        // the store's own recover() listing and delete(). Runs on a dedicated
        // OS thread + runtime because Drop is synchronous (and we may be
        // inside a tokio executor). Failures are reported to stderr but never
        // panic: a leaked test prefix should be visible, not fatal.
        let joined = std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!(
                        "warning: failed to build cleanup runtime for S3 test prefix {prefix}: {e}"
                    );
                    return;
                }
            };
            rt.block_on(async move {
                let sink = VecSink::new();
                if let Err(e) = store.recover(&sink).await {
                    eprintln!("warning: failed to list S3 test prefix {prefix}: {e}");
                    return;
                }
                for (name, _) in sink.into_inner() {
                    if let Err(e) = store.delete(&name).await {
                        eprintln!("warning: failed to delete S3 test object {prefix}/{name}: {e}");
                    }
                }
            });
        })
        .join();
        if joined.is_err() {
            eprintln!(
                "warning: cleanup thread for S3 test prefix {} panicked",
                self.prefix
            );
        }
    }
}

/// Creates an S3 bundle storage backed by a unique key prefix.
///
/// Reads:
/// - `TEST_S3_ENDPOINT` (default: `http://localhost:9000`) — MinIO or any
///   S3-compatible endpoint. Leave unset for real AWS.
/// - `TEST_S3_BUCKET` (default: `hardy-test`) — bucket name.
/// - `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` — credentials.
#[cfg(feature = "s3")]
pub async fn s3_blob_setup() -> (S3TestGuard, Arc<dyn BundleStorage>) {
    let endpoint = std::env::var("TEST_S3_ENDPOINT").ok();
    let bucket = std::env::var("TEST_S3_BUCKET").unwrap_or_else(|_| "hardy-test".to_string());
    let prefix = format!("test-{}", uuid::Uuid::new_v4().simple());

    let region = std::env::var("AWS_DEFAULT_REGION")
        .or_else(|_| std::env::var("AWS_REGION"))
        .ok()
        .or_else(|| endpoint.as_ref().map(|_| "us-east-1".to_string()));

    let mut builder = hardy_s3_storage::S3Storage::builder(bucket.clone())
        .prefix(prefix.clone())
        .force_path_style();
    if let Some(region) = region {
        builder = builder.region(region);
    }
    if let Some(endpoint) = &endpoint {
        builder = builder.endpoint_url(endpoint);
    }
    let store: Arc<dyn BundleStorage> = Arc::new(builder.build().await.unwrap_or_else(|e| {
        let endpoint = endpoint.as_deref().unwrap_or("(AWS default)");
        panic!("connect to S3/MinIO (bucket={bucket}, endpoint={endpoint}): {e}")
    }));
    (
        S3TestGuard {
            store: store.clone(),
            prefix,
        },
        store,
    )
}

// ---------------------------------------------------------------------------
// Test generation macros
// ---------------------------------------------------------------------------
//
// Each suite's test-name list is written exactly once here: every backend
// module is generated from the same list, so adding a suite function needs a
// single edit and cannot silently skip a backend.

/// Generates the core metadata suite for one backend.
#[macro_export]
macro_rules! storage_meta_tests {
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
        }
    };
}

/// Generates the recovery-protocol metadata tests for one backend.
///
/// Only applicable to persistent backends: the in-memory store's recovery
/// entry points are deliberate no-ops.
#[macro_export]
macro_rules! storage_meta_recovery_tests {
    ($mod_name:ident, $setup:path) => {
        mod $mod_name {
            use super::*;

            macro_rules! meta_recovery_test {
                ($name:ident) => {
                    #[tokio::test]
                    async fn $name() {
                        let (_cleanup, store) = $setup().await;
                        storage_tests::metadata_suite::$name(store).await;
                    }
                };
            }

            meta_recovery_test!(meta_05_confirm_exists);
            meta_recovery_test!(meta_13_remove_unconfirmed);
        }
    };
}

/// Generates the bundle (blob) suite for one backend.
#[macro_export]
macro_rules! storage_blob_tests {
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
            blob_test!(blob_05_repeatable_load);
        }
    };
}
