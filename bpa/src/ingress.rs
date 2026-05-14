//! Inbound bundle processing.
//!
//! Takes raw bytes from a CLA, produces a stored and routed `Bundle`.
//! Analogous to `BundleReader` in the streaming model.
//!
//! Pipeline: decode → filter → security → route → store.

use alloc::sync::Arc;

use bytes::Bytes;
use hardy_async::BoundedTaskPool;
use hardy_bpv7::bpsec;
use hardy_bpv7::bundle::ParsedBundle;
use hardy_bpv7::eid::NodeId;
use trace_err::*;
use tracing::debug;

use crate::bundle::{Bundle, BundleMetadata, BundleStatus, ReadOnlyMetadata};
use crate::cbor::precheck;
use crate::cla::{self, ClaAddress};
use crate::filter::{ExecResult, FilterEngine, Hook};
use crate::otel_metrics::reason_label;
use crate::rib::{FindResult, Rib};
use crate::security::KeyStore;
use crate::storage::Store;

/// The result of ingress processing.
pub(crate) struct IngressResult {
    pub bundle: Bundle,
    pub route: FindResult,
}

/// Inbound bundle processor.
///
/// Pipeline: decode → filter → security → route → store.
///
/// Configured once, called repeatedly for each bundle from a CLA.
pub(crate) struct Ingress<'a> {
    pub store: Arc<Store>,
    pub key_store: Arc<KeyStore>,
    pub filter_engine: Arc<FilterEngine>,
    pub processing_pool: &'a BoundedTaskPool,
    pub rib: Arc<Rib>,
}

impl Ingress<'_> {
    /// Process raw bytes into a stored, routed bundle.
    ///
    /// Returns `Some(IngressResult)` if accepted, `None` if dropped at any stage.
    pub async fn receive(
        &self,
        data: Bytes,
        ingress_cla: Option<Arc<str>>,
        ingress_peer_node: Option<NodeId>,
        ingress_peer_addr: Option<ClaAddress>,
    ) -> cla::Result<Option<IngressResult>> {
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

        // 1. Decode
        let Some((bundle, data)) = self.decode(&data, metadata) else {
            return Ok(None);
        };

        // 2. Filter
        let Some((mut bundle, data)) = self.filter(bundle, data).await else {
            return Ok(None);
        };

        // 3. Security (TODO: security::process_inbound)

        // 4. Route
        let route = match self.rib.find(&mut bundle) {
            FindResult::Drop(reason) => {
                debug!("Bundle dropped by routing: {reason:?}");
                metrics::counter!("bpa.bundle.received.dropped").increment(1);
                return Ok(None);
            }
            route => route,
        };

        // 5. Store
        let Some(mut bundle) = self.store_bundle(bundle, data).await? else {
            return Ok(None);
        };
        bundle.metadata.status = BundleStatus::Dispatching;
        self.store.update_metadata(&bundle).await;

        Ok(Some(IngressResult { bundle, route }))
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

    /// Run ingress filters. Returns None if dropped.
    async fn filter(&self, bundle: Bundle, data: Bytes) -> Option<(Bundle, Bytes)> {
        match self
            .filter_engine
            .exec(
                Hook::Ingress,
                bundle,
                data,
                &self.key_store,
                self.processing_pool,
            )
            .await
            .trace_expect("Ingress filter execution failed")
        {
            ExecResult::Continue(_, bundle, data) => Some((bundle, data)),
            ExecResult::Drop(_, reason) => {
                if let Some(reason) = reason {
                    metrics::counter!("bpa.bundle.dropped", "reason" => reason_label(&reason))
                        .increment(1);
                }
                None
            }
        }
    }

    /// Store bundle data and metadata. Returns None if duplicate.
    async fn store_bundle(&self, mut bundle: Bundle, data: Bytes) -> cla::Result<Option<Bundle>> {
        let storage_name = self.store.save_data(data).await;
        bundle.metadata.storage_name = Some(storage_name.clone());
        if !self.store.insert_metadata(&bundle).await {
            metrics::counter!("bpa.bundle.received.duplicate").increment(1);
            self.store.delete_data(&storage_name).await;
            return Ok(None);
        }
        Ok(Some(bundle))
    }
}
