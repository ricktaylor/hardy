use hardy_bpv7::bundle::Bundle as Bpv7Bundle;
use hardy_bpv7::eid::NodeId;
use time::OffsetDateTime;

use super::{Bundle, BundleMetadata, ReadOnlyMetadata, Stored};
use crate::cla::ClaAddress;
use crate::storage::Store;
use crate::{Arc, Bytes};

/// Typestate: bundle has been parsed and validated but not yet persisted.
pub struct Idle {
    pub(super) data: Bytes,
}

impl Bundle<Idle> {
    /// Construct a new idle bundle from a parsed BPv7 bundle, ingress context, and raw data.
    pub fn new(
        bundle: Bpv7Bundle,
        data: Bytes,
        ingress_cla: Option<Arc<str>>,
        ingress_peer_node: Option<NodeId>,
        ingress_peer_addr: Option<ClaAddress>,
    ) -> Self {
        Self {
            bundle,
            metadata: BundleMetadata {
                read_only: ReadOnlyMetadata {
                    received_at: OffsetDateTime::now_utc(),
                    ingress_peer_node,
                    ingress_peer_addr,
                    ingress_cla,
                    ..Default::default()
                },
                ..Default::default()
            },
            state: Idle { data },
        }
    }

    /// Returns a reference to the raw bundle data.
    pub fn data(&self) -> &Bytes {
        &self.state.data
    }

    /// Replace the raw bundle data (e.g. after a write filter mutates it).
    pub fn set_data(&mut self, data: Bytes) {
        self.state.data = data;
    }

    /// Persist to storage, transitioning from `Idle` to `Stored`.
    ///
    /// Safe ordering: save data first, then insert metadata.
    /// Metadata never points to missing data.
    ///
    /// Returns `None` if a bundle with the same ID already exists (duplicate).
    pub async fn store(self, store: &Store) -> Option<Bundle<Stored>> {
        let storage_name = store.save_data(&self.state.data).await;
        let inserted = store.insert_metadata(&self).await;

        if !inserted {
            store.delete_data(&storage_name).await;
            return None;
        }

        Some(Bundle {
            bundle: self.bundle,
            metadata: self.metadata,
            state: Stored { storage_name },
        })
    }
}
