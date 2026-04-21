use hardy_bpv7::bundle::RewrittenBundle;
use hardy_bpv7::status_report::ReasonCode;
use tracing::debug;
#[cfg(feature = "instrument")]
use tracing::instrument;

use super::Bpa;
use crate::bundle::{Bundle, BundleStatus, Idle, Stored};
use crate::cla;
use crate::filters::Hook;
use crate::filters::registry::ExecResult;
use crate::fragmentation::{Reassembler, ReassemblerResult};
use crate::rib::{self, FindResult};
use crate::{Arc, Bytes, Error};

impl Bpa {
    /// A bundle arrives from a CLA.
    ///
    /// Parses, stores, and queues for processing.
    /// Rejected bundles get a deletion report. Malformed CBOR errors back to CLA.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn receive(
        self: &Arc<Self>,
        data: Bytes,
        ingress_cla: Option<Arc<str>>,
        ingress_peer_node: Option<hardy_bpv7::eid::NodeId>,
        ingress_peer_addr: Option<cla::ClaAddress>,
    ) -> Result<(), Error> {
        metrics::counter!("bpa.bundle.received").increment(1);
        metrics::counter!("bpa.bundle.received.bytes").increment(data.len() as u64);

        crate::cbor::precheck(&data)?;

        let rewritten = RewrittenBundle::parse(&data, self.key_provider())?;
        let (bpv7, data, report_unsupported) = match rewritten {
            RewrittenBundle::Valid {
                bundle,
                report_unsupported,
            } => (bundle, data, report_unsupported),
            RewrittenBundle::Rewritten {
                bundle,
                new_data,
                report_unsupported,
                ..
            } => (bundle, Bytes::from(new_data), report_unsupported),
            RewrittenBundle::Invalid {
                bundle,
                reason,
                error,
            } => {
                debug!("Invalid bundle received: {error}");
                metrics::counter!("bpa.bundle.received.dropped").increment(1);
                let idle = Bundle::new(
                    bundle,
                    Bytes::new(),
                    ingress_cla,
                    ingress_peer_node,
                    ingress_peer_addr,
                );
                self.dispatcher.report_bundle_reception(&idle, reason).await;
                self.dispatcher.report_bundle_deletion(&idle, reason).await;
                return Ok(());
            }
        };

        let idle = Bundle::new(
            bpv7,
            data,
            ingress_cla,
            ingress_peer_node,
            ingress_peer_addr,
        );

        let reception_reason = report_unsupported
            .then_some(ReasonCode::BlockUnsupported)
            .unwrap_or(ReasonCode::NoAdditionalInformation);
        self.dispatcher
            .report_bundle_reception(&idle, reception_reason)
            .await;

        let bpa = Arc::clone(self);
        hardy_async::spawn!(self.processing_pool, "process_bundle", async move {
            if let Err(e) = bpa.process(idle).await {
                debug!("Bundle processing failed: {e}");
            }
        })
        .await;

        Ok(())
    }

    /// Core pipeline: filter, store, route.
    ///
    /// ```text
    /// ingress filter on in-memory data (fast reject, may mutate)
    ///   -> store (persist data + metadata)
    ///     -> route (RIB lookup)
    ///       -> forward / deliver / reassemble / admin / drop / wait
    /// ```
    #[cfg_attr(feature = "instrument", instrument(skip_all, fields(bundle.id = %bundle.bundle.id)))]
    pub(crate) async fn process(self: &Arc<Self>, bundle: Bundle<Idle>) -> Result<(), Error> {
        let data = bundle.data().clone();
        let provider = self.key_provider();
        let processing_pool = self.processing_pool;

        let ingress_result = self
            .filter_registry
            .exec(Hook::Ingress, bundle, data, provider, &processing_pool)
            .await?;

        let mut bundle = match ingress_result {
            ExecResult::Continue(_, mut bundle, data) => {
                bundle.set_data(data);
                bundle
            }
            ExecResult::Drop(bundle, reason) => {
                let reason = reason.unwrap_or(ReasonCode::NoAdditionalInformation);
                self.dispatcher
                    .report_bundle_deletion(&bundle, reason)
                    .await;
                return Ok(());
            }
        };

        let Some(mut bundle) = bundle.store(&self.store).await else {
            metrics::counter!("bpa.bundle.received.duplicate").increment(1);
            return Ok(());
        };

        bundle
            .transition(&self.store, BundleStatus::Dispatching)
            .await;

        self.route(bundle).await
    }

    /// Route a stored bundle based on RIB lookup.
    #[cfg_attr(feature = "instrument", instrument(skip_all, fields(bundle.id = %bundle.bundle.id)))]
    pub(crate) async fn route(self: &Arc<Self>, mut bundle: Bundle<Stored>) -> Result<(), Error> {
        match self.rib.find(&mut bundle) {
            Some(FindResult::Drop(reason)) => {
                debug!("Routing: bundle should be dropped: {reason:?}");
                let reason = reason.unwrap_or(ReasonCode::NoAdditionalInformation);
                self.dispatcher
                    .report_bundle_deletion(&bundle, reason)
                    .await;
                bundle.delete(&self.store).await;
            }
            Some(FindResult::AdminEndpoint) => {
                self.administrative_bundle(bundle).await;
            }
            Some(FindResult::Deliver(Some(service))) => {
                if bundle.bundle.id.fragment_info.is_some() {
                    match Reassembler::new(&self.store, self.key_provider())
                        .run(bundle)
                        .await
                    {
                        ReassemblerResult::Complete(bundle) => {
                            Box::pin(self.route(bundle)).await?;
                        }
                        ReassemblerResult::Pending | ReassemblerResult::Failed => {}
                    }
                } else {
                    if let Some(data) = bundle.get_data(&self.store).await {
                        self.dispatcher.deliver_bundle(service, bundle, data).await;
                    } else {
                        debug!("Bundle data missing from storage");
                        bundle.delete(&self.store).await;
                    }
                }
            }
            Some(FindResult::Forward(peer)) => {
                debug!("Queuing bundle for forwarding to CLA peer {peer}");
                if let Err(bundle) = self.cla_registry.forward(peer, bundle).await {
                    debug!("CLA forward failed, returning bundle to watch queue");
                    self.store.watch_bundle(bundle).await;
                }
            }
            _ => {
                debug!("Storing bundle until a forwarding opportunity arises");
                bundle.transition(&self.store, BundleStatus::Waiting).await;
                self.store.watch_bundle(bundle).await;
            }
        }

        Ok(())
    }
}
