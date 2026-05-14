use super::*;
use crate::ingress::Ingress;
use crate::rib::FindResult;

impl Dispatcher {
    pub(crate) fn ingress(&self) -> Ingress<'_> {
        Ingress {
            store: self.store.clone(),
            key_store: self.key_store.clone(),
            filter_engine: self.filter_engine.clone(),
            processing_pool: &self.processing_pool,
            rib: self.rib.clone(),
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn receive_bundle(
        &self,
        data: Bytes,
        ingress_cla: Option<Arc<str>>,
        ingress_peer_node: Option<hardy_bpv7::eid::NodeId>,
        ingress_peer_addr: Option<cla::ClaAddress>,
    ) -> cla::Result<()> {
        if let Some(result) = self
            .ingress()
            .receive(data, ingress_cla, ingress_peer_node, ingress_peer_addr)
            .await?
        {
            self.handle_route(result.bundle, result.route).await;
        }
        Ok(())
    }

    /// Handle the routing decision from ingress.
    /// Ingress already handled Drop and Wait, so only actionable routes arrive here.
    pub(super) async fn handle_route(&self, bundle: bundle::Bundle, route: FindResult) {
        match route {
            FindResult::AdminEndpoint => self.administrative_bundle(bundle).await,
            FindResult::Deliver(service) => {
                if bundle.bundle.id.fragment_info.is_some() {
                    self.reassemble(bundle).await;
                } else {
                    self.deliver_bundle(service, bundle).await;
                }
            }
            FindResult::Forward(peer) => {
                if let Err(bundle) = self.cla_registry().forward(peer, bundle).await {
                    self.store.watch_bundle(bundle).await;
                }
            }
            FindResult::Wait => {
                let mut bundle = bundle;
                self.store
                    .update_status(&mut bundle, &bundle::BundleStatus::Waiting)
                    .await;
                self.store.watch_bundle(bundle).await;
            }
            FindResult::Drop(reason) => {
                if let Some(reason) = reason {
                    self.drop_bundle(bundle, reason).await;
                } else {
                    self.delete_bundle(bundle).await;
                }
            }
        }
    }
}
