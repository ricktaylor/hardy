//! Bundle processing pipeline.
//!
//! Shared pipeline for all bundle sources: CLA, service origination, and status reports.
//! Each source provides a pre-built (Bundle, Bytes) and an optional filter Hook.
//!
//! Pipeline: [decode] → [filter] → security → route → store.

use alloc::sync::Arc;

use bytes::Bytes;
use hardy_async::BoundedTaskPool;
use hardy_bpv7::bpsec;
use hardy_bpv7::bundle::ParsedBundle;
use hardy_bpv7::eid::NodeId;
use tracing::debug;

use crate::bundle::{Bundle, BundleMetadata, BundleStatus, ReadOnlyMetadata};
use crate::cbor::precheck;
use crate::cla::ClaAddress;
use crate::filter::{ExecResult, FilterEngine, Hook};
use crate::metrics::reason_label;
use crate::rib::{FindResult, Rib};
use crate::security::KeyStore;
use crate::storage::Store;

/// The result of ingress processing.
pub(crate) enum IngressResult {
    /// Bundle accepted: stored and routed.
    Routed(Bundle, FindResult),
    /// Bundle rejected by filter or routing.
    Dropped,
    /// Duplicate bundle already in store.
    Duplicate,
}

/// Bundle processing pipeline.
///
/// Pipeline: [decode] → [filter] → security → route → store.
///
/// Owned by the BPA, shared across all bundle sources.
pub(crate) struct Ingress {
    pub store: Arc<Store>,
    pub key_store: Arc<KeyStore>,
    pub filter_engine: Arc<FilterEngine>,
    pub processing_pool: Arc<BoundedTaskPool>,
    pub rib: Arc<Rib>,
}

impl Ingress {
    /// Process raw bytes from a CLA into a stored, routed bundle.
    ///
    /// Decodes, then delegates to [`process()`](Self::process) with `Hook::Ingress`.
    pub async fn receive(
        &self,
        data: Bytes,
        ingress_cla: Option<Arc<str>>,
        ingress_peer_node: Option<NodeId>,
        ingress_peer_addr: Option<ClaAddress>,
    ) -> Result<IngressResult, crate::Error> {
        metrics::counter!("bpa.bundle.received").increment(1);
        metrics::counter!("bpa.bundle.received.bytes").increment(data.len() as u64);

        let metadata = BundleMetadata {
            status: BundleStatus::New,
            read_only: ReadOnlyMetadata {
                received_at: time::OffsetDateTime::now_utc(),
                ingress_peer_node,
                ingress_peer_addr,
                ingress_cla,
                ..Default::default()
            },
            ..Default::default()
        };

        // Decode raw bytes
        let Some((bundle, data)) = self.decode(&data, metadata) else {
            return Ok(IngressResult::Dropped);
        };

        self.process(bundle, data, Some(Hook::Ingress)).await
    }

    /// Process a pre-built bundle through the pipeline.
    ///
    /// Pipeline: [filter] → security → route → store.
    ///
    /// Used by all sources: CLA (after decode), service origination, status reports.
    /// The hook determines which filter chain runs. Pass `None` to skip filtering.
    pub async fn process(
        &self,
        bundle: Bundle,
        data: Bytes,
        hook: Option<Hook>,
    ) -> Result<IngressResult, crate::Error> {
        // 1. Filter (if hook provided)
        let (mut bundle, data) = if let Some(hook) = hook {
            let Some(result) = self.filter(bundle, data, hook).await? else {
                return Ok(IngressResult::Dropped);
            };
            result
        } else {
            (bundle, data)
        };

        // 2. Security (TODO: security::process_inbound)

        // 3. Route
        let route = match self.rib.find(&mut bundle) {
            FindResult::Drop(reason) => {
                debug!("Bundle dropped by routing: {reason:?}");
                metrics::counter!("bpa.bundle.received.dropped").increment(1);
                return Ok(IngressResult::Dropped);
            }
            route => route,
        };

        // 4. Store
        let Some(mut bundle) = self.store_bundle(bundle, data).await else {
            return Ok(IngressResult::Duplicate);
        };
        bundle.metadata.status = BundleStatus::Dispatching;
        self.store.update_metadata(&bundle).await;

        Ok(IngressResult::Routed(bundle, route))
    }

    /// Decode raw bytes into a parsed bundle.
    /// Structural parsing only: no BPSec verification or decryption.
    fn decode(&self, data: &Bytes, metadata: BundleMetadata) -> Option<(Bundle, Bytes)> {
        if let Err(e) = precheck(data) {
            debug!("Bundle rejected by CBOR precheck: {e}");
            metrics::counter!("bpa.bundle.received.dropped").increment(1);
            return None;
        }

        match ParsedBundle::parse(data, bpsec::no_keys) {
            Ok(parsed) => Some((
                Bundle {
                    metadata,
                    bundle: parsed.bundle,
                },
                data.clone(),
            )),
            Err(e) => {
                debug!("Bundle parse failed: {e}");
                metrics::counter!("bpa.bundle.received.dropped").increment(1);
                None
            }
        }
    }

    /// Run filters for the given hook. Returns None if dropped.
    async fn filter(
        &self,
        bundle: Bundle,
        data: Bytes,
        hook: Hook,
    ) -> Result<Option<(Bundle, Bytes)>, crate::Error> {
        match self
            .filter_engine
            .exec(hook, bundle, data, &self.key_store, &self.processing_pool)
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

    /// Store bundle data and metadata. Returns None if duplicate.
    async fn store_bundle(&self, mut bundle: Bundle, data: Bytes) -> Option<Bundle> {
        let storage_name = self.store.save_data(data).await;
        bundle.metadata.storage_name = Some(storage_name.clone());
        if !self.store.insert_metadata(&bundle).await {
            metrics::counter!("bpa.bundle.received.duplicate").increment(1);
            self.store.delete_data(&storage_name).await;
            return None;
        }
        Some(bundle)
    }
}
