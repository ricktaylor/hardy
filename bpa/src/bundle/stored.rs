use hardy_bpv7::eid::Eid;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use super::{Bundle, BundleStatus};
use crate::storage::Store;
use crate::{Arc, Bytes};

/// Typestate: bundle data has been persisted to storage.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Stored {
    /// Opaque key used by the storage backend to locate the serialised bundle data.
    pub storage_name: Arc<str>,
}

impl Bundle<Stored> {
    /// Load this bundle's data from storage.
    pub async fn get_data(&self, store: &Store) -> Option<Bytes> {
        store.load_data(&self.state.storage_name).await
    }

    /// Replace this bundle's stored data.
    ///
    /// Safe ordering: save new data -> update metadata -> delete old data.
    /// Metadata always points to valid data.
    pub async fn update_data(&self, store: &Store, data: &Bytes) {
        store.overwrite_data(&self.state.storage_name, data).await;
        store.update_metadata(self).await;
    }

    /// Transition the bundle's processing status and persist it.
    pub async fn transition(&mut self, store: &Store, status: BundleStatus) {
        store.update_status(self, &status).await;
    }

    /// Delete the bundle from storage (tombstone metadata, then delete data).
    ///
    /// Safe ordering: tombstone metadata first, then delete data.
    /// If crash between them: orphaned data file (harmless, cleaned by background scan).
    pub async fn delete(self, store: &Store) {
        store.tombstone_metadata(&self.bundle.id).await;
        store.delete_data(&self.state.storage_name).await;
    }

    /// Returns the EID of the node that forwarded this bundle.
    ///
    /// Prefers the Previous Node extension block (in-band), falling back to
    /// the CLA peer node ID (out-of-band). Per RFC 9171 Section 4.4.1, both
    /// identify the immediate 1-hop forwarding node when present.
    pub fn previous_node(&self) -> Option<Eid> {
        self.bundle.previous_node.clone().or_else(|| {
            self.metadata
                .read_only
                .ingress_peer_node
                .clone()
                .map(Into::into)
        })
    }
}
