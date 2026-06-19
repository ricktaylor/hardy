use core::{cmp::Ordering, fmt};

use hardy_bpv7::{
    eid::{Eid, NodeId},
    status_report::ReasonCode,
};
use hardy_eid_patterns::EidPattern;
use tracing::debug;

use super::Rib;
use crate::{Arc, HashSet, btree_map, routes, services};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Action {
    Drop(Option<ReasonCode>),
    AdminEndpoint,                           // Deliver to the admin endpoint
    Local(Arc<services::registry::Service>), // Deliver to local service
    Forward(u32),                            // Forward to a cla peer
    Reflect,
    Via(Eid),
}

impl PartialOrd for Action {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// The order is critical, do not re-order
impl Ord for Action {
    fn cmp(&self, other: &Self) -> Ordering {
        // Precedence: Drop < AdminEndpoint < Local < Forward < Reflect < Via
        let rank = |a: &Action| -> u8 {
            match a {
                Action::Drop(_) => 0,
                Action::AdminEndpoint => 1,
                Action::Local(_) => 2,
                Action::Forward(_) => 3,
                Action::Reflect => 4,
                Action::Via(_) => 5,
            }
        };
        match rank(self).cmp(&rank(other)) {
            Ordering::Equal => {}
            ord => return ord,
        }
        match (self, other) {
            (Action::Drop(a), Action::Drop(b)) => a.cmp(b),
            (Action::Local(a), Action::Local(b)) => a.cmp(b),
            (Action::Forward(a), Action::Forward(b)) => a.cmp(b),
            (Action::Via(a), Action::Via(b)) => a.cmp(b),
            _ => Ordering::Equal,
        }
    }
}

impl From<routes::Action> for Action {
    fn from(value: routes::Action) -> Self {
        match value {
            routes::Action::Drop(reason_code) => Self::Drop(reason_code),
            routes::Action::Reflect => Self::Reflect,
            routes::Action::Via(eid) => Self::Via(eid),
        }
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Action::Drop(Some(reason)) => write!(f, "Drop({reason:?})"),
            Action::Drop(None) => write!(f, "Drop"),
            Action::AdminEndpoint => write!(f, "administrative endpoint"),
            Action::Local(service) => write!(f, "local service {}", &service.service_id),
            Action::Forward(peer) => write!(f, "CLA peer {peer}"),
            Action::Reflect => write!(f, "Reflect"),
            Action::Via(eid) => write!(f, "Via {eid}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Entry {
    pub action: Action,
    pub source: String,
}

impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Entry {
    fn cmp(&self, other: &Self) -> Ordering {
        // The order is critical, hence done long-hand
        self.action
            .cmp(&other.action)
            .then_with(|| self.source.cmp(&other.source))
    }
}

impl Rib {
    pub async fn add(
        &self,
        pattern: EidPattern,
        source: String,
        action: Action,
        priority: u32,
    ) -> routes::Result<bool> {
        let pattern = if let Some(ipn) = &self.node_ids.ipn {
            pattern.expand_local_node(ipn).unwrap_or(pattern)
        } else {
            pattern
        };
        let action = match action {
            Action::Via(eid) => Action::Via(self.node_ids.expand_local_node(&eid).unwrap_or(eid)),
            other => other,
        };

        if let Action::Via(ref next_hop) = action {
            if next_hop.is_null() {
                return Err(routes::Error::NullNextHop);
            }
            if self.node_ids.is_local(next_hop) {
                return Err(routes::Error::ViaOwnNode(next_hop.clone()));
            }
        }

        let vias = {
            let new_entry = Entry {
                action: action.clone(),
                source: source.clone(),
            };

            // Scope the lock
            let mut inner = self.inner.write();
            match inner.routes.entry(priority) {
                btree_map::Entry::Vacant(e) => {
                    e.insert([(pattern.clone(), [new_entry].into())].into());
                }
                btree_map::Entry::Occupied(mut e) => match e.get_mut().entry(pattern.clone()) {
                    btree_map::Entry::Vacant(pe) => {
                        pe.insert([new_entry].into());
                    }
                    btree_map::Entry::Occupied(mut pe) => {
                        if !pe.get_mut().insert(new_entry) {
                            return Ok(false);
                        }
                    }
                },
            }

            debug!("Adding route {pattern} => {action}, priority {priority}, source '{source}'");
            metrics::gauge!("bpa.rib.entries", "source" => source).increment(1.0);

            // Start walking through the route table starting at this priority to find impacted routes
            let mut vias = HashSet::new();
            for (_, entry) in inner.routes.range(priority..) {
                for (p, actions) in entry {
                    if p.is_subset(&pattern) {
                        // We have an impacted subset, so see if we need to refresh any queue assignments
                        for entry in actions {
                            if let Action::Via(to) = &entry.action {
                                vias.insert(to.clone());
                            }
                        }
                    }
                }
            }
            vias
        };

        let changed = match action {
            Action::AdminEndpoint => false,
            Action::Local(_) | Action::Forward(_) => true,
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

    pub async fn remove(
        &self,
        pattern: &EidPattern,
        source: &str,
        action: Action,
        priority: u32,
    ) -> bool {
        let pattern = if let Some(ipn) = &self.node_ids.ipn {
            pattern
                .expand_local_node(ipn)
                .unwrap_or_else(|| pattern.clone())
        } else {
            pattern.clone()
        };
        let action = match action {
            Action::Via(eid) => Action::Via(self.node_ids.expand_local_node(&eid).unwrap_or(eid)),
            other => other,
        };

        // Remove the entry
        {
            let mut inner = self.inner.write();
            if let Some(patterns) = inner.routes.get_mut(&priority)
                && let Some(actions) = patterns.get_mut(&pattern)
                && actions.remove(&Entry {
                    action: action.clone(),
                    source: source.to_string(),
                })
            {
                if actions.is_empty() {
                    patterns.remove(&pattern);
                    if patterns.is_empty() {
                        inner.routes.remove(&priority);
                    }
                }
            } else {
                return false;
            }
        }

        debug!("Removed route {pattern} => {action}, priority {priority}, source '{source}'");
        metrics::gauge!("bpa.rib.entries", "source" => source.to_string()).decrement(1.0);

        // See if we are removing a Via or a Forward
        match action {
            Action::Via(ref to) => {
                if let Some(peers) = self.find_peers(to)
                    && self.reset_peer_queues(peers).await
                {
                    self.notify_updated().await;
                }
            }
            Action::Forward(peer) if self.store.reset_peer_queue(peer).await => {
                self.notify_updated().await;
            }
            Action::Local(_) => {
                self.notify_updated().await;
            }
            _ => {}
        }
        true
    }

    pub async fn remove_by_source(&self, source: &str) {
        let (vias, forward_peers, has_local, removed_count) = {
            let mut inner = self.inner.write();
            let mut vias = HashSet::new();
            let mut forward_peers = HashSet::new();
            let mut has_local = false;
            let mut removed_count = 0u64;

            inner.routes.retain(|_priority, patterns| {
                patterns.retain(|_pattern, actions| {
                    actions.retain(|entry| {
                        if entry.source == source {
                            match &entry.action {
                                Action::Via(to) => {
                                    vias.insert(to.clone());
                                }
                                Action::Forward(peer) => {
                                    forward_peers.insert(*peer);
                                }
                                Action::Local(_) => {
                                    has_local = true;
                                }
                                _ => {}
                            }
                            removed_count += 1;
                            false
                        } else {
                            true
                        }
                    });
                    !actions.is_empty()
                });
                !patterns.is_empty()
            });
            (vias, forward_peers, has_local, removed_count)
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
            Action::Forward(peer),
            0,
        )
        .await
    }

    /// Remove a forward route for a CLA peer.
    pub async fn remove_forward(&self, node_id: NodeId, peer: u32) -> bool {
        let pattern: EidPattern = node_id.into();
        self.remove(&pattern, Self::FORWARDS_NAME, Action::Forward(peer), 0)
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
            Action::Local(service),
            self.service_priority,
        )
        .await
    }

    /// Remove a service route for a local service.
    pub async fn remove_service(
        &self,
        eid: &Eid,
        service: Arc<services::registry::Service>,
    ) -> bool {
        let pattern: EidPattern = eid.clone().into();
        self.remove(
            &pattern,
            Self::SERVICES_NAME,
            Action::Local(service),
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
    use crate::{BTreeSet, bundle, node_ids, storage};

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

    // Add a route directly to the RIB's route table (sync, no store interaction).
    pub fn add_route(rib: &Rib, pattern: &str, source: &str, action: Action, priority: u32) {
        let pattern: EidPattern = pattern.parse().unwrap();
        let entry = Entry {
            action,
            source: source.to_string(),
        };

        let mut inner = rib.inner.write();
        match inner.routes.entry(priority) {
            btree_map::Entry::Vacant(e) => {
                e.insert([(pattern, [entry].into())].into());
            }
            btree_map::Entry::Occupied(mut e) => match e.get_mut().entry(pattern) {
                btree_map::Entry::Vacant(pe) => {
                    pe.insert([entry].into());
                }
                btree_map::Entry::Occupied(mut pe) => {
                    pe.get_mut().insert(entry);
                }
            },
        }
    }

    #[test]
    fn test_impacted_subsets() {
        let rib = make_rib();

        // Add a Via route for ipn:2.0 at priority 10
        add_route(
            &rib,
            "ipn:*.*",
            "src",
            Action::Via("ipn:0.2.0".parse().unwrap()),
            10,
        );

        // Add a more specific Drop route at priority 20 (lower priority)
        add_route(&rib, "ipn:0.3.*", "src", Action::Drop(None), 20);

        // Verify both routes were inserted
        let inner = rib.inner.read();
        assert!(inner.routes.contains_key(&10));
        assert!(inner.routes.contains_key(&20));
    }

    #[test]
    fn test_local_action_sort() {
        // AdminEndpoint < Local < Forward
        let admin = Action::AdminEndpoint;
        let forward_1 = Action::Forward(1);
        let forward_2 = Action::Forward(2);

        assert!(admin < forward_1);
        assert!(forward_1 < forward_2);

        // Verify BTreeSet ordering
        let mut set = BTreeSet::new();
        set.insert(forward_2.clone());
        set.insert(admin.clone());
        set.insert(forward_1.clone());

        let sorted: Vec<_> = set.into_iter().collect();
        assert_eq!(sorted[0], admin);
        assert_eq!(sorted[1], forward_1);
        assert_eq!(sorted[2], forward_2);
    }

    #[test]
    fn test_admin_endpoint_in_unified_table() {
        // Rib::new() inserts admin endpoint route into the unified routing
        // table at priority 0 with a concrete IPN admin EID (ipn:0.1.0).
        let rib = make_rib();

        let inner = rib.inner.read();
        let entries = inner.routes.get(&0).unwrap();

        // Should have admin endpoint for concrete ipn:0.1.0
        let admin_pattern: EidPattern = Eid::Ipn {
            fqnn: IpnNodeId {
                allocator_id: 0,
                node_number: 1,
            },
            service_number: 0,
        }
        .into();
        let admin_actions = entries.get(&admin_pattern).unwrap();
        assert!(
            admin_actions
                .iter()
                .any(|e| matches!(e.action, Action::AdminEndpoint)),
            "Concrete admin endpoint route should be in unified table"
        );
    }

    #[test]
    fn test_unregistered_local_waits() {
        // With the unified routing table, bundles destined for a local EID
        // with no registered service have no matching route — they wait (None).
        // This is the correct DTN behaviour: default to wait, not drop.
        // Operators can configure explicit Drop rules for service ranges.
        let rib = make_rib();

        // ipn:0.1.99 is under our node but no service is registered
        let mut bundle = bundle::Bundle {
            bundle: Bpv7Bundle {
                id: BundleId {
                    source: "ipn:0.99.1".parse().unwrap(),
                    timestamp: CreationTimestamp::now(),
                    fragment_info: None,
                },
                flags: Default::default(),
                crc_type: Default::default(),
                destination: "ipn:0.1.99".parse().unwrap(),
                report_to: Default::default(),
                lifetime: Duration::from_secs(3600),
                previous_node: None,
                age: None,
                hop_count: None,
                blocks: Default::default(),
            },
            metadata: Default::default(),
        };

        let result = rib.find(&mut bundle);
        assert!(
            result.is_none(),
            "Unregistered local service should wait (no route), got {result:?}"
        );
    }

    fn entry(action: Action, source: &str) -> Entry {
        Entry {
            action,
            source: source.to_string(),
        }
    }

    #[test]
    fn test_action_precedence() {
        // Drop < Reflect < Via in ordering
        let drop_entry = entry(Action::Drop(None), "a");
        let reflect_entry = entry(Action::Reflect, "a");
        let via_entry = entry(Action::Via("ipn:1.0".parse().unwrap()), "a");

        assert!(drop_entry < reflect_entry);
        assert!(reflect_entry < via_entry);
        assert!(drop_entry < via_entry);
    }

    #[test]
    fn test_route_entry_sort() {
        let mut set = BTreeSet::new();

        let via2 = entry(Action::Via("ipn:2.0".parse().unwrap()), "src1");
        let via1 = entry(Action::Via("ipn:1.0".parse().unwrap()), "src1");
        let drop_none = entry(Action::Drop(None), "src1");
        let reflect = entry(Action::Reflect, "src1");

        set.insert(via2);
        set.insert(via1);
        set.insert(drop_none);
        set.insert(reflect);

        let sorted: Vec<_> = set.into_iter().collect();
        assert!(matches!(sorted[0].action, Action::Drop(_)));
        assert!(matches!(sorted[1].action, Action::Reflect));
        assert!(matches!(sorted[2].action, Action::Via(_)));
        assert!(matches!(sorted[3].action, Action::Via(_)));
    }

    #[test]
    fn test_entry_source_tiebreak() {
        // Same action, different source — sorted by source name
        let a = entry(Action::Reflect, "alpha");
        let b = entry(Action::Reflect, "beta");
        assert!(a < b);
    }

    #[test]
    fn test_entry_dedup() {
        let mut set = BTreeSet::new();
        let e1 = entry(Action::Reflect, "src");
        let e2 = entry(Action::Reflect, "src");
        assert!(set.insert(e1));
        assert!(!set.insert(e2)); // duplicate
    }

    #[tokio::test]
    async fn test_reject_null_next_hop() {
        let rib = make_rib();
        let result = rib
            .add(
                "ipn:0.2.*".parse().unwrap(),
                "test".into(),
                Action::Via(Eid::Null),
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
                Action::Via("ipn:0.1.0".parse().unwrap()),
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
                Action::Via("ipn:0.2.0".parse().unwrap()),
                10,
            )
            .await;
        assert!(
            matches!(result, Ok(true)),
            "Default route should be accepted, got {result:?}"
        );
    }
}
