//! Bundle recovery after BPA restart.
//!
//! Validates stored bundles and resumes processing based on their
//! checkpoint status.

use alloc::sync::Arc;

use hardy_bpv7::bundle::ParsedBundle;
use tracing::{debug, warn};

use crate::bundle::{Bundle, BundleMetadata, BundleStatus, ReadOnlyMetadata};
use crate::otel_metrics::status_label;
use crate::security::KeyStore;
use crate::storage::Store;
use crate::storage::channel::Sender;

/// Recover a single bundle from storage after restart.
pub(crate) async fn recover_bundle(
    storage_name: Arc<str>,
    file_time: time::OffsetDateTime,
    store: &Store,
    key_store: &Arc<KeyStore>,
    dispatch_tx: &Sender,
) {
    let Some(data) = store.load_data(&storage_name).await else {
        return;
    };

    let keys = key_store.current();
    let parsed = match ParsedBundle::parse_with_keys(&data, &**keys) {
        Ok(parsed) => parsed.bundle,
        Err(e) => {
            warn!("Corrupt bundle data found: {storage_name}, {e}");
            store.delete_data(&storage_name).await;
            metrics::counter!("bpa.restart.junk").increment(1);
            return;
        }
    };

    if let Some(metadata) = store.confirm_exists(&parsed.id).await {
        if metadata.storage_name.as_ref() != Some(&storage_name) {
            if metadata.storage_name.is_none() {
                warn!("Duplicate copy of processed bundle data found: {storage_name}");
            } else {
                warn!(
                    "Duplicate bundle data found: {storage_name} != {:?}",
                    metadata.storage_name.as_ref()
                );
            }
            store.delete_data(&storage_name).await;
            metrics::counter!("bpa.restart.duplicate").increment(1);
            return;
        }

        let bundle = Bundle {
            metadata,
            bundle: parsed,
        };
        match &bundle.metadata.status {
            BundleStatus::New | BundleStatus::Dispatching => {
                metrics::gauge!("bpa.bundle.status", "state" => status_label(&bundle.metadata.status)).increment(1.0);
                dispatch(dispatch_tx, bundle).await;
            }
            BundleStatus::ForwardPending { .. } => {
                let mut bundle = bundle;
                metrics::gauge!("bpa.bundle.status", "state" => status_label(&bundle.metadata.status)).increment(1.0);
                store
                    .update_status(&mut bundle, &BundleStatus::Waiting)
                    .await;
            }
            _ => {
                metrics::gauge!("bpa.bundle.status", "state" => status_label(&bundle.metadata.status)).increment(1.0);
            }
        }
    } else {
        let metadata = BundleMetadata {
            status: BundleStatus::Dispatching,
            storage_name: Some(storage_name),
            read_only: ReadOnlyMetadata {
                received_at: file_time,
                ..Default::default()
            },
            ..Default::default()
        };

        let bundle = Bundle {
            metadata,
            bundle: parsed,
        };
        store.insert_metadata(&bundle).await;
        metrics::counter!("bpa.restart.orphan").increment(1);
        dispatch(dispatch_tx, bundle).await;
    }
}

async fn dispatch(dispatch_tx: &Sender, bundle: Bundle) {
    if dispatch_tx.send(bundle).await.is_err() {
        debug!("Dispatch queue closed during recovery");
    }
}
