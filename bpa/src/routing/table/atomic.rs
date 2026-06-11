use std::collections::HashSet;
use std::sync::Arc;

use hardy_bpv7::eid::Eid;
use hardy_eid_patterns::EidPattern;
use tracing::{trace, warn};

use crate::node_ids::NodeIds;
use crate::services;

use super::{
    Action, Entries, FindResult, InternalAction, RouteAction, VirtualRouteTable, sorted_insert,
};

#[cfg(feature = "instrument")]
use tracing::instrument;

pub(crate) struct AtomicRouteTable {
    pub(super) entries: Entries,
}

impl AtomicRouteTable {
    pub(crate) fn new(node_ids: &NodeIds) -> Self {
        let mut entries: Entries = Default::default();
        let mut admin_entries = Vec::new();

        if let Some(node_name) = &node_ids.dtn {
            let admin_eid: Eid = node_name.clone().into();
            admin_entries.push((
                admin_eid.into(),
                Action::Internal(InternalAction::AdminEndpoint),
                "administrative endpoint".to_string(),
            ));
        }

        if let Some(node_number) = &node_ids.ipn {
            let admin_eid: Eid = (*node_number).into();
            admin_entries.push((
                admin_eid.into(),
                Action::Internal(InternalAction::AdminEndpoint),
                "administrative endpoint".to_string(),
            ));
        }

        if !admin_entries.is_empty() {
            entries.insert(0, admin_entries);
        }

        Self { entries }
    }

    pub(crate) fn virtual_table<'a>(
        &self,
        source: &str,
        node_ids: &'a NodeIds,
    ) -> VirtualRouteTable<'a> {
        VirtualRouteTable::new(self.entries.clone(), source, node_ids)
    }

    pub(crate) fn insert(
        &mut self,
        pattern: EidPattern,
        action: InternalAction,
        priority: u32,
        source: &str,
    ) {
        let entry = (pattern, Action::Internal(action), source.to_string());
        self.entries.entry(priority).or_default().push(entry);
    }

    pub(crate) fn remove(
        &mut self,
        pattern: &EidPattern,
        action: &InternalAction,
        priority: u32,
        source: &str,
    ) -> bool {
        let target = Action::Internal(action.clone());
        if let Some(entries) = self.entries.get_mut(&priority) {
            let before = entries.len();
            entries.retain(|(p, a, s)| !(p == pattern && *a == target && s == source));
            if entries.len() < before {
                if entries.is_empty() {
                    self.entries.remove(&priority);
                }
                return true;
            }
        }
        false
    }

    pub(crate) fn remove_by_source(&mut self, source: &str) -> usize {
        let mut removed = 0;
        self.entries.retain(|_, entries| {
            let before = entries.len();
            entries.retain(|(_, _, s)| s != source);
            removed += before - entries.len();
            !entries.is_empty()
        });
        removed
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, to, trail), fields(to = %to)))]
    pub(crate) fn find_recurse<'a>(
        &'a self,
        to: &'a Eid,
        reflect: bool,
        trail: &mut HashSet<&'a Eid>,
    ) -> Option<FindResult<'a>> {
        trace!("Looking for route for {to}");

        let mut peers: Vec<(u32, &'a Eid)> = Vec::new();
        for entries in self.entries.values() {
            for (pattern, action, _source) in entries {
                if pattern.matches(to) {
                    match action {
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
                                warn!("Skipping recursive route for {to}");
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
            }
            if !peers.is_empty() {
                return Some(FindResult::Forward(peers));
            }
        }
        None
    }

    pub(crate) fn find_service(&self, to: &Eid) -> Option<Arc<services::registry::Service>> {
        for entries in self.entries.values() {
            for (pattern, action, _) in entries {
                if pattern.matches(to) {
                    if let Action::Internal(InternalAction::Local(service)) = action {
                        return Some(service.clone());
                    }
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::super::*;
    use crate::node_ids::NodeIds;
    use hardy_bpv7::eid::IpnNodeId;

    fn node_ids() -> NodeIds {
        NodeIds {
            ipn: Some(IpnNodeId {
                allocator_id: 0,
                node_number: 1,
            }),
            dtn: None,
        }
    }

    fn make_table() -> AtomicRouteTable {
        AtomicRouteTable::new(&node_ids())
    }

    #[test]
    fn admin_endpoint_at_construction() {
        let table = make_table();
        let entries = table.entries.get(&0).unwrap();
        assert!(
            entries
                .iter()
                .any(|(_, a, _)| matches!(a, Action::Internal(InternalAction::AdminEndpoint)))
        );
    }

    #[test]
    fn insert_and_remove_internal() {
        let mut table = make_table();
        let pattern: EidPattern = "ipn:0.2.*".parse().unwrap();

        table.insert(
            pattern.clone(),
            InternalAction::Forward(42),
            0,
            "neighbours",
        );

        let entries = table.entries.get(&0).unwrap();
        assert!(
            entries.iter().any(|(p, a, _)| p == &pattern
                && matches!(a, Action::Internal(InternalAction::Forward(42))))
        );

        assert!(table.remove(&pattern, &InternalAction::Forward(42), 0, "neighbours"));
        let has_forward = table.entries.get(&0).is_some_and(|e| {
            e.iter()
                .any(|(_, a, _)| matches!(a, Action::Internal(InternalAction::Forward(42))))
        });
        assert!(!has_forward);
    }

    #[test]
    fn remove_by_source_cleans_up() {
        let mut table = make_table();
        table.insert(
            "ipn:0.2.*".parse().unwrap(),
            InternalAction::Forward(42),
            0,
            "test_source",
        );
        table.insert(
            "ipn:0.3.*".parse().unwrap(),
            InternalAction::Forward(77),
            0,
            "test_source",
        );

        let removed = table.remove_by_source("test_source");
        assert_eq!(removed, 2);
    }

    #[test]
    fn find_recurse_forward() {
        let mut table = make_table();
        table.insert(
            "ipn:0.2.*".parse().unwrap(),
            InternalAction::Forward(42),
            0,
            "neighbours",
        );

        let dest: Eid = "ipn:0.2.1".parse().unwrap();
        let result = table.find_recurse(&dest, true, &mut HashSet::new());
        assert!(matches!(result, Some(FindResult::Forward(_))));
    }

    #[test]
    fn find_recurse_via_chain() {
        let ids = node_ids();
        let mut table = make_table();
        table.insert(
            "ipn:0.3.*".parse().unwrap(),
            InternalAction::Forward(77),
            0,
            "neighbours",
        );

        let mut vt = table.virtual_table("test", &ids);
        vt.insert(
            "ipn:0.50.*".parse().unwrap(),
            RouteAction::Via("ipn:0.3.0".parse().unwrap()),
            10,
        )
        .unwrap();
        let table = vt.commit().unwrap();

        let dest: Eid = "ipn:0.50.1".parse().unwrap();
        let result = table.find_recurse(&dest, true, &mut HashSet::new());
        assert!(matches!(result, Some(FindResult::Forward(_))));
    }

    #[test]
    fn find_recurse_drop() {
        let ids = node_ids();
        let table = make_table();
        let mut vt = table.virtual_table("test", &ids);
        vt.insert("ipn:0.99.*".parse().unwrap(), RouteAction::Drop(None), 10)
            .unwrap();
        let table = vt.commit().unwrap();

        let dest: Eid = "ipn:0.99.1".parse().unwrap();
        let result = table.find_recurse(&dest, true, &mut HashSet::new());
        assert!(matches!(result, Some(FindResult::Drop(None))));
    }

    #[test]
    fn find_recurse_no_route() {
        let table = make_table();
        let dest: Eid = "ipn:0.50.1".parse().unwrap();
        let result = table.find_recurse(&dest, true, &mut HashSet::new());
        assert!(result.is_none());
    }

    #[test]
    fn find_service_works() {
        let mut table = make_table();
        let svc = Arc::new(services::registry::Service {
            service: services::registry::ServiceImpl::LowLevel(Arc::new(
                crate::services::tests::NullService,
            )),
            service_id: hardy_bpv7::eid::Service::Ipn(42),
        });
        table.insert(
            "ipn:0.1.42".parse().unwrap(),
            InternalAction::Local(svc),
            1,
            "services",
        );

        let eid: Eid = "ipn:0.1.42".parse().unwrap();
        assert!(table.find_service(&eid).is_some());

        let other: Eid = "ipn:0.1.99".parse().unwrap();
        assert!(table.find_service(&other).is_none());
    }
}
