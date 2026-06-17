use core::{cmp::Ordering, fmt};

use hardy_bpv7::{
    eid::{Eid, NodeId},
    status_report::ReasonCode,
};
use hardy_eid_patterns::EidPattern;
use tracing::debug;

use super::Rib;
use crate::{Arc, HashSet, btree_map, services};

use hardy_bpv7::eid::{Eid, NodeId};
use hardy_eid_patterns::EidPattern;
use tracing::debug;

use super::action::{Action, InternalAction, RouteAction};
use super::rib::Rib;
use super::table::Entry;
use crate::services;
use crate::{Arc, HashSet};

impl Rib {
    pub(crate) async fn add(
        &self,
        pattern: EidPattern,
        source: String,
        action: Action,
        priority: u32,
    ) -> routes::Result<bool> {
        let vias = {
            let entry = Entry {
                action: action.clone(),
                source: source.clone(),
            };

            let mut inner = self.inner.write();
            if !inner.table.insert(pattern.clone(), entry, priority) {
                return false;
            }

            debug!("Adding route {pattern} => {action}, priority {priority}, source '{source}'");
            metrics::gauge!("bpa.rib.entries", "source" => source).increment(1.0);

            inner.table.impacted_vias(&pattern, priority)
        };

        let changed = match action {
            Action::Internal(InternalAction::AdminEndpoint) => false,
            Action::Internal(InternalAction::Local(_))
            | Action::Internal(InternalAction::Forward(_)) => true,
            _ => {
                let mut changed = false;
                for v in vias {
                    if let Some(peers) = self.find_peers(&v)
                        && self.reset_peer_queues(peers).await
                    {
                        changed = true;
                    }
                }
                changed
            }
        };
        if changed {
            self.notify_updated().await;
        }
        Ok(true)
    }

    pub(crate) async fn remove(
        &self,
        pattern: &EidPattern,
        source: &str,
        action: Action,
        priority: u32,
    ) -> bool {
        {
            let entry = Entry {
                action: action.clone(),
                source: source.to_string(),
            };
            let mut inner = self.inner.write();
            if !inner.table.remove(pattern, &entry, priority) {
                return false;
            }
        }

        debug!("Removed route {pattern} => {action}, priority {priority}, source '{source}'");
        metrics::gauge!("bpa.rib.entries", "source" => source.to_string()).decrement(1.0);

        match action {
            Action::Route(RouteAction::Via(ref to)) => {
                if let Some(peers) = self.find_peers(to)
                    && self.reset_peer_queues(peers).await
                {
                    self.notify_updated().await;
                }
            }
            Action::Internal(InternalAction::Forward(peer))
                if self.store.reset_peer_queue(peer).await =>
            {
                self.notify_updated().await;
            }
            Action::Internal(InternalAction::Local(_)) => {
                self.notify_updated().await;
            }
            _ => {}
        }
        true
    }

    pub async fn remove_by_source(&self, source: &str) {
        let (vias, forward_peers, has_local, removed_count) = {
            let mut inner = self.inner.write();
            inner.table.remove_by_source(source)
        };

        if removed_count == 0 {
            return;
        }

        debug!("Removed all routes from source '{source}'");
        metrics::gauge!("bpa.rib.entries", "source" => source.to_string())
            .decrement(removed_count as f64);

        let mut changed = has_local;
        for v in vias {
            if let Some(peers) = self.find_peers(&v)
                && self.reset_peer_queues(peers).await
            {
                changed = true;
            }
        }
        for peer in forward_peers {
            if self.store.reset_peer_queue(peer).await {
                changed = true;
            }
        }
        if changed {
            self.notify_updated().await;
        }
    }

    async fn reset_peer_queues(&self, peers: HashSet<u32>) -> bool {
        let mut updated = false;
        for p in peers {
            if self.store.reset_peer_queue(p).await {
                updated = true;
            }
        }
        updated
    }

    /// Add a forward route for a CLA peer.
    /// The NodeId is converted to a wildcard pattern (e.g., ipn:1.* for all services).
    pub async fn add_forward(&self, node_id: NodeId, peer: u32) -> routes::Result<bool> {
        let pattern: EidPattern = node_id.into();
        self.add(
            pattern,
            Self::FORWARDS_NAME.into(),
            Action::Internal(InternalAction::Forward(peer)),
            0,
        )
        .await
    }

    pub async fn remove_forward(&self, node_id: NodeId, peer: u32) -> bool {
        let pattern: EidPattern = node_id.into();
        self.remove(
            &pattern,
            Self::FORWARDS_NAME,
            Action::Internal(InternalAction::Forward(peer)),
            0,
        )
        .await
    }

    /// Add a service route for a local service.
    pub async fn add_service(
        &self,
        eid: Eid,
        service: Arc<services::registry::Service>,
    ) -> routes::Result<bool> {
        self.add(
            eid.into(),
            Self::SERVICES_NAME.into(),
            Action::Internal(InternalAction::Local(service)),
            self.service_priority,
        )
        .await
    }

    pub async fn remove_service(
        &self,
        eid: &Eid,
        service: Arc<services::registry::Service>,
    ) -> bool {
        let pattern: EidPattern = eid.clone().into();
        self.remove(
            &pattern,
            Self::SERVICES_NAME,
            Action::Internal(InternalAction::Local(service)),
            self.service_priority,
        )
        .await
    }
}

#[cfg(test)]
pub(super) mod tests {
    use core::{num::NonZeroUsize, time::Duration};

    use hardy_bpv7::{
        bundle::{Bundle as Bpv7Bundle, Id as BundleId},
        creation_timestamp::CreationTimestamp,
        eid::IpnNodeId,
    };

    use super::*;
    use crate::{node_ids, storage};

    pub fn make_rib() -> Arc<Rib> {
        let node_ids = Arc::new(node_ids::NodeIds {
            ipn: Some(IpnNodeId {
                allocator_id: 0,
                node_number: 1,
            }),
            dtn: None,
        });

        let store = Arc::new(storage::store::Store::new(
            NonZeroUsize::new(16).unwrap(),
            Arc::new(storage::MetadataMemStorage::new(&Default::default())),
            Arc::new(storage::BundleMemStorage::new(&Default::default())),
        ));

        Arc::new(Rib::new(node_ids, store, 1))
    }

    pub fn add_route(rib: &Rib, pattern: &str, source: &str, action: Action, priority: u32) {
        let pattern: EidPattern = pattern.parse().unwrap();
        let entry = Entry {
            action,
            source: source.to_string(),
        };

        let mut inner = rib.inner.write();
        inner.table.insert(pattern, entry, priority);
    }

    #[tokio::test]
    async fn test_reject_null_next_hop() {
        let rib = make_rib();
        let result = rib
            .add(
                "ipn:0.2.*".parse().unwrap(),
                "test".into(),
                Action::Route(RouteAction::Via(hardy_bpv7::eid::Eid::Null)),
                10,
            )
            .await;
        assert!(
            matches!(result, Err(routes::Error::NullNextHop)),
            "Via null endpoint should be rejected, got {result:?}"
        );
    }

    #[tokio::test]
    async fn test_reject_via_own_node() {
        let rib = make_rib();
        let result = rib
            .add(
                "ipn:0.99.*".parse().unwrap(),
                "test".into(),
                Action::Route(RouteAction::Via("ipn:0.1.0".parse().unwrap())),
                10,
            )
            .await;
        assert!(
            matches!(result, Err(routes::Error::ViaOwnNode(_))),
            "Via own node should be rejected, got {result:?}"
        );
    }

    #[tokio::test]
    async fn test_allow_default_route() {
        let rib = make_rib();
        let result = rib
            .add(
                "*:**".parse().unwrap(),
                "test".into(),
                Action::Route(RouteAction::Via("ipn:0.2.0".parse().unwrap())),
                10,
            )
            .await;
        assert!(
            matches!(result, Ok(true)),
            "Default route should be accepted, got {result:?}"
        );
    }
}
