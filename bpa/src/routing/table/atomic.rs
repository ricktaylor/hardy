use std::collections::HashSet;
use std::sync::Arc;

use arc_swap::ArcSwap;
use hardy_bpv7::eid::Eid;
use hardy_eid_patterns::EidPattern;
use tracing::{trace, warn};

use super::{
    Action, Entries, Error, FindResult, InternalAction, RouteAction, VirtualRouteTable,
    sorted_insert,
};
use crate::node_ids::NodeIds;
use crate::routing::route::Route;
use crate::services;

#[cfg(feature = "instrument")]
use tracing::instrument;

pub(crate) struct AtomicRouteTable {
    entries: ArcSwap<Entries>,
    write_lock: hardy_async::sync::spin::Mutex<()>,
    node_ids: Arc<NodeIds>,
}

impl AtomicRouteTable {
    pub(crate) fn new(node_ids: Arc<NodeIds>) -> Self {
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

        Self {
            entries: ArcSwap::from_pointee(entries),
            write_lock: hardy_async::sync::spin::Mutex::new(()),
            node_ids,
        }
    }

    pub(crate) fn update_routes(
        &self,
        source: &str,
        add: &[Route],
        remove: &[Route],
    ) -> Result<(), Error> {
        let _guard = self.write_lock.lock();
        let current = self.entries.load();
        let mut vt = VirtualRouteTable::new((**current).clone(), source, &self.node_ids);
        for r in remove {
            vt.remove(&r.pattern, &r.action, r.priority);
        }
        for r in add {
            vt.insert(r.pattern.clone(), r.action.clone(), r.priority)?;
        }
        let new_entries = vt.commit()?;
        self.entries.store(Arc::new(new_entries));
        Ok(())
    }

    pub(crate) fn insert(
        &self,
        pattern: EidPattern,
        action: InternalAction,
        priority: u32,
        source: &str,
    ) {
        let _guard = self.write_lock.lock();
        let mut entries = (**self.entries.load()).clone();
        let entry = (pattern, Action::Internal(action), source.to_string());
        entries.entry(priority).or_default().push(entry);
        self.entries.store(Arc::new(entries));
    }

    pub(crate) fn remove(
        &self,
        pattern: &EidPattern,
        action: &InternalAction,
        priority: u32,
        source: &str,
    ) -> bool {
        let _guard = self.write_lock.lock();
        let target = Action::Internal(action.clone());
        let mut entries = (**self.entries.load()).clone();
        if let Some(at_priority) = entries.get_mut(&priority) {
            let before = at_priority.len();
            at_priority.retain(|(p, a, s)| !(p == pattern && *a == target && s == source));
            if at_priority.len() < before {
                if at_priority.is_empty() {
                    entries.remove(&priority);
                }
                self.entries.store(Arc::new(entries));
                return true;
            }
        }
        false
    }

    pub(crate) fn remove_by_source(&self, source: &str) -> usize {
        let _guard = self.write_lock.lock();
        let mut entries = (**self.entries.load()).clone();
        let mut removed = 0;
        entries.retain(|_, at_priority| {
            let before = at_priority.len();
            at_priority.retain(|(_, _, s)| s != source);
            removed += before - at_priority.len();
            !at_priority.is_empty()
        });
        if removed > 0 {
            self.entries.store(Arc::new(entries));
        }
        removed
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, to), fields(to = %to)))]
    pub(crate) fn find_recurse(&self, to: &Eid, reflect: bool) -> Option<FindResult> {
        let entries = self.entries.load();
        Self::find_recurse_inner(&entries, to, reflect, &mut HashSet::new())
    }

    fn find_recurse_inner(
        entries: &Entries,
        to: &Eid,
        reflect: bool,
        trail: &mut HashSet<Eid>,
    ) -> Option<FindResult> {
        trace!("Looking for route for {to}");

        let mut peers: Vec<(u32, Eid)> = Vec::new();
        for at_priority in entries.values() {
            for (pattern, action, _source) in at_priority {
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
                            if !trail.insert(to.clone()) {
                                warn!("Skipping recursive route for {to}");
                                continue;
                            }

                            let sub_result = Self::find_recurse_inner(entries, via, reflect, trail);
                            trail.remove(to);

                            if let Some(sub_result) = sub_result {
                                let FindResult::Forward(sub_peers) = sub_result else {
                                    return Some(sub_result);
                                };
                                for (sub_peer, _) in sub_peers {
                                    sorted_insert(&mut peers, sub_peer, via.clone());
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
                            sorted_insert(&mut peers, *peer, to.clone());
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
        let entries = self.entries.load();
        for at_priority in entries.values() {
            for (pattern, action, _) in at_priority {
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
    use super::super::*;
    use crate::node_ids::NodeIds;
    use crate::routing::route::Route;
    use hardy_bpv7::eid::IpnNodeId;
    use std::sync::Arc;

    fn node_ids() -> Arc<NodeIds> {
        Arc::new(NodeIds {
            ipn: Some(IpnNodeId {
                allocator_id: 0,
                node_number: 1,
            }),
            dtn: None,
        })
    }

    fn make_table() -> AtomicRouteTable {
        AtomicRouteTable::new(node_ids())
    }

    #[test]
    fn admin_endpoint_at_construction() {
        let table = make_table();
        let entries = table.entries.load();
        let at_zero = entries.get(&0).unwrap();
        assert!(
            at_zero
                .iter()
                .any(|(_, a, _)| matches!(a, Action::Internal(InternalAction::AdminEndpoint)))
        );
    }

    #[test]
    fn insert_and_remove_internal() {
        let table = make_table();
        let pattern: EidPattern = "ipn:0.2.*".parse().unwrap();

        table.insert(
            pattern.clone(),
            InternalAction::Forward(42),
            0,
            "neighbours",
        );

        {
            let entries = table.entries.load();
            let at_zero = entries.get(&0).unwrap();
            assert!(at_zero.iter().any(|(p, a, _)| p == &pattern
                && matches!(a, Action::Internal(InternalAction::Forward(42)))));
        }

        assert!(table.remove(&pattern, &InternalAction::Forward(42), 0, "neighbours"));
        {
            let entries = table.entries.load();
            let has_forward = entries.get(&0).is_some_and(|e| {
                e.iter()
                    .any(|(_, a, _)| matches!(a, Action::Internal(InternalAction::Forward(42))))
            });
            assert!(!has_forward);
        }
    }

    #[test]
    fn remove_by_source_cleans_up() {
        let table = make_table();
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
        let table = make_table();
        table.insert(
            "ipn:0.2.*".parse().unwrap(),
            InternalAction::Forward(42),
            0,
            "neighbours",
        );

        let dest: Eid = "ipn:0.2.1".parse().unwrap();
        let result = table.find_recurse(&dest, true);
        assert!(matches!(result, Some(FindResult::Forward(_))));
    }

    #[test]
    fn find_recurse_via_chain() {
        let table = make_table();
        table.insert(
            "ipn:0.3.*".parse().unwrap(),
            InternalAction::Forward(77),
            0,
            "neighbours",
        );

        table
            .update_routes(
                "test",
                &[Route::via(
                    "ipn:0.50.*".parse().unwrap(),
                    "ipn:0.3.0".parse().unwrap(),
                    10,
                )],
                &[],
            )
            .unwrap();

        let dest: Eid = "ipn:0.50.1".parse().unwrap();
        let result = table.find_recurse(&dest, true);
        assert!(matches!(result, Some(FindResult::Forward(_))));
    }

    #[test]
    fn find_recurse_drop() {
        let table = make_table();
        table
            .update_routes(
                "test",
                &[Route::drop("ipn:0.99.*".parse().unwrap(), None, 10)],
                &[],
            )
            .unwrap();

        let dest: Eid = "ipn:0.99.1".parse().unwrap();
        let result = table.find_recurse(&dest, true);
        assert!(matches!(result, Some(FindResult::Drop(None))));
    }

    #[test]
    fn find_recurse_no_route() {
        let table = make_table();
        let dest: Eid = "ipn:0.50.1".parse().unwrap();
        let result = table.find_recurse(&dest, true);
        assert!(result.is_none());
    }

    #[test]
    fn find_service_works() {
        let table = make_table();
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
