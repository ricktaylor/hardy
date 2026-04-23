use super::*;

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct Entry {
    pub action: routes::Action,
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
            (routes::Action::Drop(lhs), routes::Action::Drop(rhs)) => lhs.cmp(rhs),
            (routes::Action::Drop(_), routes::Action::Reflect)
            | (routes::Action::Drop(_), routes::Action::Via(_)) => core::cmp::Ordering::Less,
            (routes::Action::Reflect, routes::Action::Drop(_)) => core::cmp::Ordering::Greater,
            (routes::Action::Reflect, routes::Action::Reflect) => core::cmp::Ordering::Equal,
            (routes::Action::Reflect, routes::Action::Via(_)) => core::cmp::Ordering::Less,
            (routes::Action::Via(_), routes::Action::Drop(_))
            | (routes::Action::Via(_), routes::Action::Reflect) => core::cmp::Ordering::Greater,
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

            debug!("Adding route {pattern} => {action}, priority {priority}, source '{source}'");
            metrics::gauge!("bpa.rib.entries", "source" => source.clone()).increment(1.0);

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

        debug!("Removed route {pattern} => {action}, priority {priority}, source '{source}'");
        metrics::gauge!("bpa.rib.entries", "source" => source.to_string()).decrement(1.0);

        // See if we are removing a Via
        if let routes::Action::Via(to) = action
            && let Some(peers) = self.find_peers(to)
            && self.reset_peer_queues(peers).await
        {
            self.notify_updated().await;
        }
        true
    }

    pub async fn remove_by_source(&self, source: &str) {
        let (vias, removed_count) = {
            let mut inner = self.inner.write();
            let mut vias = HashSet::new();
            let mut removed_count = 0u64;

            inner.routes.retain(|_priority, patterns| {
                patterns.retain(|_pattern, actions| {
                    actions.retain(|entry| {
                        if entry.source == source {
                            if let routes::Action::Via(to) = &entry.action {
                                vias.insert(to.clone());
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
            (vias, removed_count)
        };

        if removed_count > 0 {
            metrics::gauge!("bpa.rib.entries", "source" => source.to_string())
                .decrement(removed_count as f64);
        }

        if vias.is_empty() {
            return;
        }

        debug!("Removed all routes from source '{source}'");

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
    }

    async fn reset_peer_queues(&self, _peers: HashSet<u32>) -> bool {
        // No-op: ForwardPending status no longer exists.
        // Bundles in Waiting status are re-routed by the cold path poll.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(action: routes::Action, source: &str) -> Entry {
        Entry {
            action,
            source: source.to_string(),
        }
    }

    #[test]
    fn test_action_precedence() {
        // Drop < Reflect < Via in ordering
        let drop_entry = entry(routes::Action::Drop(None), "a");
        let reflect_entry = entry(routes::Action::Reflect, "a");
        let via_entry = entry(routes::Action::Via("ipn:1.0".parse().unwrap()), "a");

        assert!(drop_entry < reflect_entry);
        assert!(reflect_entry < via_entry);
        assert!(drop_entry < via_entry);
    }

    #[test]
    fn test_route_entry_sort() {
        let mut set = BTreeSet::new();

        let via2 = entry(routes::Action::Via("ipn:2.0".parse().unwrap()), "src1");
        let via1 = entry(routes::Action::Via("ipn:1.0".parse().unwrap()), "src1");
        let drop_none = entry(routes::Action::Drop(None), "src1");
        let reflect = entry(routes::Action::Reflect, "src1");

        set.insert(via2);
        set.insert(via1);
        set.insert(drop_none);
        set.insert(reflect);

        let sorted: Vec<_> = set.into_iter().collect();
        assert!(matches!(sorted[0].action, routes::Action::Drop(_)));
        assert!(matches!(sorted[1].action, routes::Action::Reflect));
        assert!(matches!(sorted[2].action, routes::Action::Via(_)));
        assert!(matches!(sorted[3].action, routes::Action::Via(_)));
    }

    #[test]
    fn test_entry_source_tiebreak() {
        // Same action, different source — sorted by source name
        let a = entry(routes::Action::Reflect, "alpha");
        let b = entry(routes::Action::Reflect, "beta");
        assert!(a < b);
    }

    #[test]
    fn test_entry_dedup() {
        let mut set = BTreeSet::new();
        let e1 = entry(routes::Action::Reflect, "src");
        let e2 = entry(routes::Action::Reflect, "src");
        assert!(set.insert(e1));
        assert!(!set.insert(e2)); // duplicate
    }
}
