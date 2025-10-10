use super::*;

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct Entry {
    pub action: routes::Action,
    pub source: String,
}

impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Entry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // The order is critical, hence done long-hand
        match (&self.action, &other.action) {
            (routes::Action::Drop(lhs), routes::Action::Drop(rhs)) => lhs.cmp(rhs),
            (routes::Action::Drop(_), routes::Action::Reflect)
            | (routes::Action::Drop(_), routes::Action::Via(_)) => std::cmp::Ordering::Less,
            (routes::Action::Reflect, routes::Action::Drop(_)) => std::cmp::Ordering::Greater,
            (routes::Action::Reflect, routes::Action::Reflect) => std::cmp::Ordering::Equal,
            (routes::Action::Reflect, routes::Action::Via(_)) => std::cmp::Ordering::Less,
            (routes::Action::Via(_), routes::Action::Drop(_))
            | (routes::Action::Via(_), routes::Action::Reflect) => std::cmp::Ordering::Greater,
            (routes::Action::Via(lhs), routes::Action::Via(rhs)) => lhs.cmp(rhs),
        }
        .then_with(|| self.source.cmp(&other.source))
    }
}

impl Rib {
    pub async fn add(
        &self,
        pattern: EidPattern,
        source: String,
        action: routes::Action,
        priority: u32,
    ) -> bool {
        let vias = {
            let new_entry = Entry {
                action: action.clone(),
                source: source.clone(),
            };

            // Scope the lock
            let mut inner = self.inner.write().trace_expect("Failed to lock mutex");
            match inner.routes.entry(priority) {
                std::collections::btree_map::Entry::Vacant(e) => {
                    e.insert([(pattern.clone(), [new_entry].into())].into());
                }
                std::collections::btree_map::Entry::Occupied(mut e) => {
                    match e.get_mut().entry(pattern.clone()) {
                        std::collections::btree_map::Entry::Vacant(pe) => {
                            pe.insert([new_entry].into());
                        }
                        std::collections::btree_map::Entry::Occupied(mut pe) => {
                            if !pe.get_mut().insert(new_entry) {
                                return false;
                            }
                        }
                    }
                }
            }

            info!("Adding route {pattern} => {action}, priority {priority}, source '{source}'");

            // Start walking through the route table starting at this priority to find impacted routes
            let mut vias = HashSet::new();
            for (_, entry) in inner.routes.range(priority..) {
                for (p, actions) in entry {
                    if p.is_subset(&pattern) {
                        // We have an impacted subset, so see if we need to refresh any queue assignments
                        for entry in actions {
                            if let routes::Action::Via(to) = &entry.action {
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
        action: &routes::Action,
        priority: u32,
    ) -> bool {
        // Remove the entry
        {
            let mut inner = self.inner.write().trace_expect("Failed to lock mutex");
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

        info!("Removed route {pattern} => {action}, priority {priority}, source '{source}'");

        // See if we are removing a Via
        if let routes::Action::Via(to) = action
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
