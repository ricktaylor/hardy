use std::sync::Arc;

use bytes::Bytes;
use hardy_bpa::bundle;
use hardy_bpa::bundle::{BundleMetadata, BundleStatus};
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
        Arc::new(hardy_bpa::storage::MetadataMemStorage::new(
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
        Arc::new(hardy_bpa::storage::BundleMemStorage::new(
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
