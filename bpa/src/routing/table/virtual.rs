use std::collections::HashSet;

use hardy_bpv7::eid::Eid;
use hardy_eid_patterns::EidPattern;

use crate::node_ids::NodeIds;

use super::{Action, Entries, Error, RouteAction};

pub(crate) struct VirtualRouteTable<'a> {
    entries: Entries,
    node_ids: &'a NodeIds,
    source: String,
}

impl<'a> VirtualRouteTable<'a> {
    pub(super) fn new(entries: Entries, source: &str, node_ids: &'a NodeIds) -> Self {
        Self {
            entries,
            node_ids,
            source: source.to_string(),
        }
    }

    pub(crate) fn insert(
        &mut self,
        pattern: EidPattern,
        action: RouteAction,
        priority: u32,
    ) -> Result<bool, Error> {
        let pattern = if let Some(ipn) = &self.node_ids.ipn {
            pattern.expand_local_node(ipn).unwrap_or(pattern)
        } else {
            pattern
        };

        let action = match action {
            RouteAction::Via(eid) => {
                RouteAction::Via(self.node_ids.expand_local_node(&eid).unwrap_or(eid))
            }
            other => other,
        };

        self.validate(&pattern, &action)?;

        let entry = (pattern, Action::Route(action), self.source.clone());

        let entries = self.entries.entry(priority).or_default();
        if entries.contains(&entry) {
            return Ok(false);
        }
        entries.push(entry);
        Ok(true)
    }

    pub(crate) fn remove(
        &mut self,
        pattern: &EidPattern,
        action: &RouteAction,
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
            RouteAction::Via(eid) => Action::Route(RouteAction::Via(
                self.node_ids
                    .expand_local_node(eid)
                    .unwrap_or_else(|| eid.clone()),
            )),
            other => Action::Route(other.clone()),
        };

        if let Some(entries) = self.entries.get_mut(&priority) {
            let before = entries.len();
            entries.retain(|(p, a, s)| !(p == &pattern && *a == action && s == &self.source));
            if entries.len() < before {
                if entries.is_empty() {
                    self.entries.remove(&priority);
                }
                return true;
            }
        }
        false
    }

    pub(crate) fn commit(self) -> Result<Entries, Error> {
        self.validate_table()?;
        Ok(self.entries)
    }

    fn validate(&self, pattern: &EidPattern, action: &RouteAction) -> Result<(), Error> {
        match action {
            RouteAction::Via(next_hop) => {
                if next_hop.is_null() {
                    return Err(Error::NullNextHop {
                        pattern: pattern.clone(),
                    });
                }
                if pattern.matches(next_hop) {
                    return Err(Error::SelfReferential {
                        pattern: pattern.clone(),
                        next_hop: next_hop.clone(),
                    });
                }
                if self.node_ids.is_local(next_hop) {
                    return Err(Error::ViaOwnNode {
                        pattern: pattern.clone(),
                        next_hop: next_hop.clone(),
                    });
                }
            }
            RouteAction::Reflect => {
                let matches_self = self
                    .node_ids
                    .ipn
                    .is_some_and(|ipn| pattern.matches(&ipn.into()))
                    || self
                        .node_ids
                        .dtn
                        .as_ref()
                        .is_some_and(|dtn| pattern.matches(&dtn.clone().into()));
                if matches_self {
                    return Err(Error::ReflectMatchesSelf {
                        pattern: pattern.clone(),
                    });
                }
            }
            RouteAction::Drop(_) => {}
        }
        Ok(())
    }

    fn validate_table(&self) -> Result<(), Error> {
        for entries in self.entries.values() {
            for (_pattern, action, _) in entries {
                if let Action::Route(RouteAction::Via(via)) = action {
                    let mut trail = HashSet::new();
                    trail.insert(via.clone());
                    self.check_via_chain(via, &mut trail)?;
                }
            }
        }
        Ok(())
    }

    fn check_via_chain(&self, target: &Eid, trail: &mut HashSet<Eid>) -> Result<(), Error> {
        for entries in self.entries.values() {
            for (pattern, action, _) in entries {
                if pattern.matches(target) {
                    if let Action::Route(RouteAction::Via(next)) = action {
                        if !trail.insert(next.clone()) {
                            let chain: Vec<Eid> = trail.iter().cloned().collect();
                            return Err(Error::TransitiveLoop { chain });
                        }
                        self.check_via_chain(next, trail)?;
                        trail.remove(next);
                    }
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
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

    fn make_virtual(ids: &NodeIds) -> VirtualRouteTable<'_> {
        VirtualRouteTable::new(Default::default(), "test", ids)
    }

    fn p(s: &str) -> EidPattern {
        s.parse().unwrap()
    }

    #[test]
    fn insert_validates_null_hop() {
        let ids = node_ids();
        let mut vt = make_virtual(&ids);

        let result = vt.insert(p("ipn:0.2.*"), RouteAction::Via(Eid::Null), 10);
        assert!(matches!(result, Err(Error::NullNextHop { .. })));
    }

    #[test]
    fn insert_validates_self_referential() {
        let ids = node_ids();
        let mut vt = make_virtual(&ids);

        let result = vt.insert(
            p("ipn:0.2.*"),
            RouteAction::Via("ipn:0.2.0".parse().unwrap()),
            10,
        );
        assert!(matches!(result, Err(Error::SelfReferential { .. })));
    }

    #[test]
    fn insert_validates_via_own_node() {
        let ids = node_ids();
        let mut vt = make_virtual(&ids);

        let result = vt.insert(
            p("ipn:0.99.*"),
            RouteAction::Via("ipn:0.1.0".parse().unwrap()),
            10,
        );
        assert!(matches!(result, Err(Error::ViaOwnNode { .. })));
    }

    #[test]
    fn insert_validates_reflect_self() {
        let ids = node_ids();
        let mut vt = make_virtual(&ids);

        let result = vt.insert(p("ipn:0.1.*"), RouteAction::Reflect, 10);
        assert!(matches!(result, Err(Error::ReflectMatchesSelf { .. })));
    }

    #[test]
    fn valid_insert_and_commit() {
        let ids = node_ids();
        let mut vt = make_virtual(&ids);

        let inserted = vt
            .insert(
                p("ipn:0.50.*"),
                RouteAction::Via("ipn:0.2.0".parse().unwrap()),
                10,
            )
            .unwrap();
        assert!(inserted);

        let entries = vt.commit().unwrap();
        assert!(entries.contains_key(&10));
    }

    #[test]
    fn duplicate_returns_false() {
        let ids = node_ids();
        let mut vt = make_virtual(&ids);

        let via = RouteAction::Via("ipn:0.2.0".parse().unwrap());
        vt.insert(p("ipn:0.50.*"), via.clone(), 10).unwrap();

        let dup = vt.insert(p("ipn:0.50.*"), via, 10).unwrap();
        assert!(!dup);
    }

    #[test]
    fn commit_detects_transitive_loop() {
        let ids = node_ids();
        let mut vt = make_virtual(&ids);

        vt.insert(
            p("ipn:0.2.*"),
            RouteAction::Via("ipn:0.3.0".parse().unwrap()),
            10,
        )
        .unwrap();
        vt.insert(
            p("ipn:0.3.*"),
            RouteAction::Via("ipn:0.2.0".parse().unwrap()),
            10,
        )
        .unwrap();

        let result = vt.commit();
        assert!(matches!(result, Err(Error::TransitiveLoop { .. })));
    }

    #[test]
    fn commit_allows_non_looping_chain() {
        let ids = node_ids();
        let mut entries: Entries = Default::default();
        entries.entry(0).or_default().push((
            "ipn:0.3.*".parse().unwrap(),
            Action::Internal(InternalAction::Forward(77)),
            "neighbours".to_string(),
        ));

        let mut vt = VirtualRouteTable::new(entries, "test", &ids);
        vt.insert(
            p("ipn:0.2.*"),
            RouteAction::Via("ipn:0.3.0".parse().unwrap()),
            10,
        )
        .unwrap();

        let entries = vt.commit().unwrap();
        assert!(entries.contains_key(&10));
    }

    #[test]
    fn scoped_to_source() {
        let ids = node_ids();

        let mut vt = VirtualRouteTable::new(Default::default(), "agent_a", &ids);
        vt.insert(
            p("ipn:0.50.*"),
            RouteAction::Via("ipn:0.2.0".parse().unwrap()),
            10,
        )
        .unwrap();
        let entries = vt.commit().unwrap();

        let mut vt = VirtualRouteTable::new(entries, "agent_b", &ids);
        let removed = vt.remove(
            &p("ipn:0.50.*"),
            &RouteAction::Via("ipn:0.2.0".parse().unwrap()),
            10,
        );
        assert!(!removed);
    }
}
