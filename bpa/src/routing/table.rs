use hardy_bpv7::eid::Eid;
use hardy_bpv7::status_report::ReasonCode;
use hardy_eid_patterns::EidPattern;
use tracing::{trace, warn};

#[cfg(feature = "instrument")]
use tracing::instrument;

use super::action::{Action, InternalAction, RouteAction};
use crate::node_ids::NodeIds;
use crate::services::registry::Service;
use crate::{Arc, BTreeMap, BTreeSet, HashSet, btree_map};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Entry {
    pub action: Action,
    pub source: String,
}

impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Entry {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.action
            .cmp(&other.action)
            .then_with(|| self.source.cmp(&other.source))
    }
}

pub(crate) struct RouteTable {
    routes: BTreeMap<u32, BTreeMap<EidPattern, BTreeSet<Entry>>>,
    node_ids: Arc<NodeIds>,
}

impl RouteTable {
    pub(crate) fn new(node_ids: Arc<NodeIds>) -> Self {
        let entry = Entry {
            source: "administrative endpoint".into(),
            action: Action::Internal(InternalAction::AdminEndpoint),
        };

        let mut admin_endpoints = BTreeMap::new();
        if let Some(node_name) = &node_ids.dtn {
            let admin_eid: Eid = node_name.clone().into();
            admin_endpoints.insert(admin_eid.into(), [entry.clone()].into());
        }

        if let Some(node_number) = &node_ids.ipn {
            let admin_eid: Eid = (*node_number).into();
            admin_endpoints.insert(admin_eid.into(), [entry].into());
        }

        let mut routes = BTreeMap::new();
        routes.insert(0, admin_endpoints);

        Self { routes, node_ids }
    }

    fn expand_pattern(&self, pattern: EidPattern) -> EidPattern {
        if let Some(ipn) = &self.node_ids.ipn {
            pattern.expand_local_node(ipn).unwrap_or(pattern)
        } else {
            pattern
        }
    }

    fn expand_action(&self, action: Action) -> Action {
        match action {
            Action::Route(RouteAction::Via(eid)) => Action::Route(RouteAction::Via(
                self.node_ids.expand_local_node(&eid).unwrap_or(eid),
            )),
            other => other,
        }
    }

    fn validate(&self, pattern: &EidPattern, action: &Action, source: &str) -> bool {
        if let Action::Route(RouteAction::Via(next_hop)) = action {
            if next_hop.is_null() {
                warn!(
                    "Rejecting route with null next-hop: {pattern} via {next_hop} (source '{source}')"
                );
                return false;
            }
            if self.node_ids.is_local(next_hop) {
                warn!("Rejecting route via own node: {pattern} via {next_hop} (source '{source}')");
                return false;
            }
        }
        true
    }

    pub(super) fn insert(&mut self, pattern: EidPattern, entry: Entry, priority: u32) -> bool {
        let pattern = self.expand_pattern(pattern);
        let entry = Entry {
            action: self.expand_action(entry.action),
            source: entry.source,
        };

        if !self.validate(&pattern, &entry.action, &entry.source) {
            return false;
        }

        match self.routes.entry(priority) {
            btree_map::Entry::Vacant(e) => {
                e.insert([(pattern, [entry].into())].into());
            }
            btree_map::Entry::Occupied(mut e) => match e.get_mut().entry(pattern) {
                btree_map::Entry::Vacant(pe) => {
                    pe.insert([entry].into());
                }
                btree_map::Entry::Occupied(mut pe) => {
                    if !pe.get_mut().insert(entry) {
                        return false;
                    }
                }
            },
        }
        true
    }

    pub(super) fn remove(&mut self, pattern: &EidPattern, entry: &Entry, priority: u32) -> bool {
        let pattern = self.expand_pattern(pattern.clone());
        let entry = Entry {
            action: self.expand_action(entry.action.clone()),
            source: entry.source.clone(),
        };

        if let Some(patterns) = self.routes.get_mut(&priority)
            && let Some(actions) = patterns.get_mut(&pattern)
            && actions.remove(&entry)
        {
            if actions.is_empty() {
                patterns.remove(&pattern);
                if patterns.is_empty() {
                    self.routes.remove(&priority);
                }
            }
            true
        } else {
            false
        }
    }

    pub(super) fn remove_by_source(
        &mut self,
        source: &str,
    ) -> (HashSet<Eid>, HashSet<u32>, bool, u64) {
        let mut vias = HashSet::new();
        let mut forward_peers = HashSet::new();
        let mut has_local = false;
        let mut removed_count = 0u64;

        self.routes.retain(|_priority, patterns| {
            patterns.retain(|_pattern, actions| {
                actions.retain(|entry| {
                    if entry.source == source {
                        match &entry.action {
                            Action::Route(RouteAction::Via(to)) => {
                                vias.insert(to.clone());
                            }
                            Action::Internal(InternalAction::Forward(peer)) => {
                                forward_peers.insert(*peer);
                            }
                            Action::Internal(InternalAction::Local(_)) => {
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
    }

    pub(super) fn impacted_vias(&self, pattern: &EidPattern, priority: u32) -> HashSet<Eid> {
        let mut vias = HashSet::new();
        for (_, entry) in self.routes.range(priority..) {
            for (p, actions) in entry {
                if p.is_subset(pattern) {
                    for entry in actions {
                        if let Action::Route(RouteAction::Via(to)) = &entry.action {
                            vias.insert(to.clone());
                        }
                    }
                }
            }
        }
        vias
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, to, trail), fields(to = %to)))]
    pub(super) fn find_recurse<'a>(
        &'a self,
        to: &'a Eid,
        reflect: bool,
        trail: &mut HashSet<&'a Eid>,
    ) -> Option<FindResult<'a>> {
        trace!("Looking for route for {to}");

        let mut peers: Vec<(u32, &'a Eid)> = Vec::new();
        for entries in self.routes.values() {
            for (pattern, actions) in entries {
                if pattern.matches(to) {
                    for entry in actions {
                        match &entry.action {
                            Action::Route(RouteAction::Drop(reason)) => {
                                trace!("Drop {reason:?}");
                                return Some(FindResult::Drop(*reason));
                            }
                            Action::Route(RouteAction::Reflect) => {
                                if reflect {
                                    trace!("Reflect");
                                    return Some(FindResult::Reflect);
                                }
                            }
                            Action::Route(RouteAction::Via(via)) => {
                                if !trail.insert(to) {
                                    trace!("Skipping recursive route for {to}");
                                    continue;
                                }

                                let sub_result = self.find_recurse(via, reflect, trail);
                                trail.remove(&to);

                                if let Some(sub_result) = sub_result {
                                    let FindResult::Forward(sub_peers) = sub_result else {
                                        return Some(sub_result);
                                    };
                                    for (sub_peer, _) in sub_peers {
                                        sorted_insert(&mut peers, sub_peer, via);
                                    }
                                }
                            }
                            Action::Internal(InternalAction::AdminEndpoint) => {
                                trace!("Deliver to Admin Endpoint");
                                return Some(FindResult::AdminEndpoint);
                            }
                            Action::Internal(InternalAction::Local(service)) => {
                                trace!("Deliver to Service {}", service.service_id);
                                return Some(FindResult::Deliver(service.clone()));
                            }
                            Action::Internal(InternalAction::Forward(peer)) => {
                                sorted_insert(&mut peers, *peer, to);
                            }
                        }
                    }

                    if !peers.is_empty() {
                        return Some(FindResult::Forward(peers));
                    }
                }
            }
        }
        None
    }

    pub(super) fn find_peers(&self, to: &Eid) -> Option<HashSet<u32>> {
        if let Some(FindResult::Forward(peers)) = self.find_recurse(to, false, &mut HashSet::new())
        {
            Some(peers.into_iter().map(|(peer, _)| peer).collect())
        } else {
            None
        }
    }

    pub(super) fn find_service(&self, to: &Eid) -> Option<Arc<Service>> {
        for entries in self.routes.values() {
            for (pattern, actions) in entries {
                if pattern.matches(to) {
                    for entry in actions {
                        if let Action::Internal(InternalAction::Local(service)) = &entry.action {
                            return Some(service.clone());
                        }
                    }
                }
            }
        }
        None
    }
}

#[derive(Debug)]
pub(super) enum FindResult<'a> {
    AdminEndpoint,
    Deliver(Arc<Service>),
    Forward(Vec<(u32, &'a Eid)>),
    Drop(Option<ReasonCode>),
    Reflect,
}

fn sorted_insert<'a>(peers: &mut Vec<(u32, &'a Eid)>, peer: u32, next_hop: &'a Eid) {
    if let Err(idx) = peers.binary_search_by_key(&peer, |(p, _)| *p) {
        peers.insert(idx, (peer, next_hop));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_ids::NodeIds;
    use hardy_bpv7::eid::IpnNodeId;

    fn make_table() -> RouteTable {
        RouteTable::new(Arc::new(NodeIds {
            ipn: Some(IpnNodeId {
                allocator_id: 0,
                node_number: 1,
            }),
            dtn: None,
        }))
    }

    fn entry(action: Action, source: &str) -> Entry {
        Entry {
            action,
            source: source.to_string(),
        }
    }

    #[test]
    fn test_admin_endpoint_at_construction() {
        let table = make_table();
        let entries = table.routes.get(&0).unwrap();

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
                .any(|e| matches!(e.action, Action::Internal(InternalAction::AdminEndpoint))),
        );
    }

    #[test]
    fn test_insert_and_remove() {
        let mut table = make_table();
        let e = entry(Action::Internal(InternalAction::Forward(42)), "neighbours");
        assert!(table.insert("ipn:0.2.*".parse().unwrap(), e.clone(), 0));

        assert!(table.remove(&"ipn:0.2.*".parse().unwrap(), &e, 0));
    }

    #[test]
    fn test_impacted_subsets() {
        let mut table = make_table();

        table.insert(
            "ipn:*.*".parse().unwrap(),
            entry(
                Action::Route(RouteAction::Via("ipn:0.2.0".parse().unwrap())),
                "src",
            ),
            10,
        );
        table.insert(
            "ipn:0.3.*".parse().unwrap(),
            entry(Action::Route(RouteAction::Drop(None)), "src"),
            20,
        );

        assert!(table.routes.contains_key(&10));
        assert!(table.routes.contains_key(&20));
    }

    #[test]
    fn test_local_action_sort() {
        let admin = Action::Internal(InternalAction::AdminEndpoint);
        let forward_1 = Action::Internal(InternalAction::Forward(1));
        let forward_2 = Action::Internal(InternalAction::Forward(2));

        assert!(admin < forward_1);
        assert!(forward_1 < forward_2);

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
    fn test_action_precedence() {
        let drop_entry = entry(Action::Route(RouteAction::Drop(None)), "a");
        let reflect_entry = entry(Action::Route(RouteAction::Reflect), "a");
        let via_entry = entry(
            Action::Route(RouteAction::Via("ipn:1.0".parse().unwrap())),
            "a",
        );

        assert!(drop_entry < reflect_entry);
        assert!(reflect_entry < via_entry);
        assert!(drop_entry < via_entry);
    }

    #[test]
    fn test_route_entry_sort() {
        let mut set = BTreeSet::new();

        set.insert(entry(
            Action::Route(RouteAction::Via("ipn:2.0".parse().unwrap())),
            "src1",
        ));
        set.insert(entry(
            Action::Route(RouteAction::Via("ipn:1.0".parse().unwrap())),
            "src1",
        ));
        set.insert(entry(Action::Route(RouteAction::Drop(None)), "src1"));
        set.insert(entry(Action::Route(RouteAction::Reflect), "src1"));

        let sorted: Vec<_> = set.into_iter().collect();
        assert!(matches!(
            sorted[0].action,
            Action::Route(RouteAction::Drop(_))
        ));
        assert!(matches!(
            sorted[1].action,
            Action::Route(RouteAction::Reflect)
        ));
        assert!(matches!(
            sorted[2].action,
            Action::Route(RouteAction::Via(_))
        ));
        assert!(matches!(
            sorted[3].action,
            Action::Route(RouteAction::Via(_))
        ));
    }

    #[test]
    fn test_entry_source_tiebreak() {
        let a = entry(Action::Route(RouteAction::Reflect), "alpha");
        let b = entry(Action::Route(RouteAction::Reflect), "beta");
        assert!(a < b);
    }

    #[test]
    fn test_entry_dedup() {
        let mut set = BTreeSet::new();
        let e1 = entry(Action::Route(RouteAction::Reflect), "src");
        let e2 = entry(Action::Route(RouteAction::Reflect), "src");
        assert!(set.insert(e1));
        assert!(!set.insert(e2));
    }

    #[test]
    fn test_validate_null_next_hop() {
        let mut table = make_table();
        let result = table.insert(
            "ipn:0.2.*".parse().unwrap(),
            entry(Action::Route(RouteAction::Via(Eid::Null)), "test"),
            10,
        );
        assert!(!result, "Via null endpoint should be rejected");
    }

    #[test]
    fn test_validate_via_own_node() {
        let mut table = make_table();
        let result = table.insert(
            "ipn:0.99.*".parse().unwrap(),
            entry(
                Action::Route(RouteAction::Via("ipn:0.1.0".parse().unwrap())),
                "test",
            ),
            10,
        );
        assert!(!result, "Via own node should be rejected");
    }

    #[test]
    fn test_allow_default_route() {
        let mut table = make_table();
        let result = table.insert(
            "*:**".parse().unwrap(),
            entry(
                Action::Route(RouteAction::Via("ipn:0.2.0".parse().unwrap())),
                "test",
            ),
            10,
        );
        assert!(result, "Default route should be accepted");
    }
}
