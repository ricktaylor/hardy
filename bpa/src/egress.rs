//! Outbound bundle processing.
//!
//! Prepares a stored bundle and writes it to a `Sink`.
//! Analogous to `BundleWriter` in the streaming model.
//!
//! Pipeline: load → decode → egress hook (filter/mutate/security) → encode → write.

use alloc::sync::Arc;

use bytes::Bytes;
use hardy_async::BoundedTaskPool;
use trace_err::*;

use crate::bundle::Bundle;
use crate::filter::{ExecResult, FilterEngine, Hook};
use crate::metrics::reason_label;
use crate::security::KeyStore;
use crate::sink::Sink;
use crate::storage::Store;

/// Result of egress processing.
pub(crate) enum SendResult {
    /// Bundle processed and written to sink.
    Sent,
    /// Sink rejected the bundle.
    Rejected,
    /// Bundle dropped by egress filter.
    Filtered,
    /// Bundle data not found in storage.
    NotFound,
}

/// Outbound bundle processor.
///
/// Pipeline: load → decode → egress hook → encode → write to sink.
///
/// Owned by the BPA, shared across all outbound destinations.
pub(crate) struct Egress {
    pub store: Arc<Store>,
    pub key_store: Arc<KeyStore>,
    pub filter_engine: Arc<FilterEngine>,
    pub processing_pool: Arc<BoundedTaskPool>,
}

impl Egress {
    /// Process a stored bundle and write it to the sink.
    pub async fn send(&self, bundle: Bundle, sink: &dyn Sink) -> Result<SendResult, crate::Error> {
        // 1. Load
        let Some(data) = self.load_data(&bundle).await else {
            return Ok(SendResult::NotFound);
        };

        // 2. Decode (TODO: re-parse for fresh bundle structure)

        // 3. Egress hook: filters and mutations (ext blocks, security, user filters)
        let Some((bundle, data)) = self.filter(bundle, data).await? else {
            return Ok(SendResult::Filtered);
        };

        // 4. Write to sink
        match sink.write(&bundle, data).await {
            Ok(()) => {
                metrics::counter!("bpa.bundle.forwarded").increment(1);
                Ok(SendResult::Sent)
            }
            Err(_) => Ok(SendResult::Rejected),
        }
    }

    /// Run egress filters. Returns None if dropped.
    async fn filter(
        &self,
        bundle: Bundle,
        data: Bytes,
    ) -> Result<Option<(Bundle, Bytes)>, crate::Error> {
        match self
            .filter_engine
            .exec(
                Hook::Egress,
                bundle,
                data,
                &self.key_store,
                &self.processing_pool,
            )
            .await?
        {
            ExecResult::Continue(_, bundle, data) => Ok(Some((bundle, data))),
            ExecResult::Drop(_, reason) => {
                if let Some(reason) = reason {
                    metrics::counter!("bpa.bundle.dropped", "reason" => reason_label(&reason))
                        .increment(1);
                }
                Ok(None)
            }
        }
    }

    async fn load_data(&self, bundle: &Bundle) -> Option<Bytes> {
        let storage_name = bundle
            .metadata
            .storage_name
            .as_ref()
            .trace_expect("Bundle without storage_name reached egress");

        self.store.load_data(storage_name).await
    }
}
