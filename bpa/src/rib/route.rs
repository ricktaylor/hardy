use hardy_eid_patterns::EidPattern;

use super::Rib;
use crate::routes::Action as RouteAction;
use crate::{HashSet, btree_map};

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct Entry {
    pub action: RouteAction,
    pub source: String,
}

impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Entry {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        // The order is critical, hence done long-hand
        match (&self.action, &other.action) {
            (RouteAction::Drop(lhs), RouteAction::Drop(rhs)) => lhs.cmp(rhs),
            (RouteAction::Drop(_), RouteAction::Reflect)
            | (RouteAction::Drop(_), RouteAction::Via(_)) => core::cmp::Ordering::Less,
            (RouteAction::Reflect, RouteAction::Drop(_)) => core::cmp::Ordering::Greater,
            (RouteAction::Reflect, RouteAction::Reflect) => core::cmp::Ordering::Equal,
            (RouteAction::Reflect, RouteAction::Via(_)) => core::cmp::Ordering::Less,
            (RouteAction::Via(_), RouteAction::Drop(_))
            | (RouteAction::Via(_), RouteAction::Reflect) => core::cmp::Ordering::Greater,
            (RouteAction::Via(lhs), RouteAction::Via(rhs)) => lhs.cmp(rhs),
        }
        .then_with(|| self.source.cmp(&other.source))
    }
}

impl Rib {
    pub async fn add(
        &self,
        pattern: EidPattern,
        source: String,
        action: RouteAction,
        priority: u32,
    ) -> bool {
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
                            return false;
                        }
                    }
                },
            }

            tracing::debug!(
                "Adding route {pattern} => {action}, priority {priority}, source '{source}'"
            );

            // Start walking through the route table starting at this priority to find impacted routes
            let mut vias = HashSet::new();
            for (_, entry) in inner.routes.range(priority..) {
                for (p, actions) in entry {
                    if p.is_subset(&pattern) {
                        // We have an impacted subset, so see if we need to refresh any queue assignments
                        for entry in actions {
                            if let RouteAction::Via(to) = &entry.action {
                                vias.insert(to.clone());
                            }
                        }
                    }
                }
            }
            vias
        };

        let mut changed = false;
        for v in vias {
            if let Some(peers) = self.find_peers(&v)
                && self.reset_peer_queues(peers).await
            {
                changed = true;
            }
        }
        if changed {
            self.notify_updated().await;
        }
        true
    }

    pub async fn remove(
        &self,
        pattern: &EidPattern,
        source: &str,
        action: &RouteAction,
        priority: u32,
    ) -> bool {
        // Remove the entry
        {
            let mut inner = self.inner.write();
            if let Some(patterns) = inner.routes.get_mut(&priority)
                && let Some(actions) = patterns.get_mut(pattern)
                && actions.remove(&Entry {
                    action: action.clone(),
                    source: source.to_string(),
                })
            {
                if actions.is_empty() {
                    patterns.remove(pattern);
                    if patterns.is_empty() {
                        inner.routes.remove(&priority);
                    }
                }
            } else {
                return false;
            }
        }

        tracing::debug!(
            "Removed route {pattern} => {action}, priority {priority}, source '{source}'"
        );

        // See if we are removing a Via
        if let RouteAction::Via(to) = action
            && let Some(peers) = self.find_peers(to)
            && self.reset_peer_queues(peers).await
        {
            self.notify_updated().await;
        }
        true
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
}

#[cfg(test)]
mod tests {
    // use super::*;

    // // TODO: Implement test for 'Action Precedence' (Verify Drop takes precedence over Via)
    // #[test]
    // fn test_action_precedence() {
    //     todo!("Verify Drop takes precedence over Via");
    // }

    // // TODO: Implement test for 'Route Entry Sort' (Verify Ord impl for route::Entry)
    // #[test]
    // fn test_route_entry_sort() {
    //     todo!("Verify Ord impl for route::Entry");
    // }
}
