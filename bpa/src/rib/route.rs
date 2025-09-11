use super::*;
use hardy_eid_pattern::EidPattern;

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct Entry {
    pub priority: u32,
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
        self.priority
            .cmp(&other.priority)
            .then_with(|| match (&self.action, &other.action) {
                (routes::Action::Drop(lhs), routes::Action::Drop(rhs)) => lhs.cmp(rhs),
                (routes::Action::Drop(_), routes::Action::Reflect)
                | (routes::Action::Drop(_), routes::Action::Via(_)) => std::cmp::Ordering::Less,
                (routes::Action::Reflect, routes::Action::Drop(_)) => std::cmp::Ordering::Greater,
                (routes::Action::Reflect, routes::Action::Reflect) => std::cmp::Ordering::Equal,
                (routes::Action::Reflect, routes::Action::Via(_)) => std::cmp::Ordering::Less,
                (routes::Action::Via(_), routes::Action::Drop(_))
                | (routes::Action::Via(_), routes::Action::Reflect) => std::cmp::Ordering::Greater,
                (routes::Action::Via(lhs), routes::Action::Via(rhs)) => lhs.cmp(rhs),
            })
            .reverse()
        // BinaryHeap is a max-heap!
    }
}

impl Rib {
    pub async fn add(
        &self,
        pattern: EidPattern,
        source: String,
        action: routes::Action,
        priority: u32,
    ) {
        info!("Adding route {pattern} => {action}, priority {priority}, source '{source}'");

        self.inner
            .write()
            .trace_expect("Failed to lock mutex")
            .routes
            .insert(
                &pattern,
                Entry {
                    source,
                    action,
                    priority,
                },
            );

        // TODO: Re-evaluate NoRoute and Pending bundles
    }

    pub fn remove(
        &self,
        pattern: &EidPattern,
        source: &str,
        action: &routes::Action,
        priority: u32,
    ) -> bool {
        // Do a pattern lookup to get Via's

        // Do a recursive find for each Via to get Peer Id

        let v = {
            self.inner
                .write()
                .trace_expect("Failed to lock mutex")
                .routes
                .remove_if::<Vec<_>>(pattern, |e| {
                    e.source == source && e.priority == priority && &e.action == action
                })
        };

        // TODO: Re-evaluate NoRoute and Pending bundles

        for v in &v {
            info!(
                "Removed route {pattern} => {}, priority {priority}, source '{source}'",
                v.action
            )
        }

        !v.is_empty()
    }
}
