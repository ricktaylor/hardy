use super::*;
use hardy_eid_pattern::EidPatternSet;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Action {
    AdminEndpoint,                         // Deliver to the admin endpoint
    Local(Arc<service_registry::Service>), // Deliver to local service
    Forward(u32),                          // Forward to CLA peer
}

impl PartialOrd for Action {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Action {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // The order is critical, hence done long-hand
        match (self, other) {
            (Action::AdminEndpoint, Action::AdminEndpoint) => std::cmp::Ordering::Equal,
            (Action::AdminEndpoint, Action::Local(_))
            | (Action::AdminEndpoint, Action::Forward(..)) => std::cmp::Ordering::Less,
            (Action::Local(_), Action::AdminEndpoint) => std::cmp::Ordering::Greater,
            (Action::Local(lhs), Action::Local(rhs)) => lhs.cmp(rhs),
            (Action::Local(_), Action::Forward(..)) => std::cmp::Ordering::Less,
            (Action::Forward(_), Action::AdminEndpoint)
            | (Action::Forward(_), Action::Local(_)) => std::cmp::Ordering::Greater,
            (Action::Forward(lhs), Action::Forward(rhs)) => lhs.cmp(rhs),
        }
        .reverse()
        // BinaryHeap is a max-heap!
    }
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Action::AdminEndpoint => write!(f, "administrative endpoint"),
            Action::Local(service) => write!(f, "local service {}", &service.service_id),
            Action::Forward(peer_id) => {
                write!(f, "forward to peer {peer_id}")
            }
        }
    }
}

pub struct LocalInner {
    pub actions: HashMap<Eid, BinaryHeap<local::Action>>,
    pub finals: EidPatternSet,
}

impl LocalInner {
    pub fn new(config: &config::Config) -> Self {
        let mut actions = HashMap::new();
        let mut finals = EidPatternSet::new();

        // Add localnode admin endpoint
        actions.insert(
            Eid::LocalNode { service_number: 0 },
            vec![local::Action::AdminEndpoint].into(),
        );

        // Drop LocalNode services
        finals.insert(&"ipn:!.*".parse().unwrap());

        if let Some((allocator_id, node_number)) = config.node_ids.ipn {
            // Add the Admin Endpoint EID itself
            actions.insert(
                Eid::Ipn {
                    allocator_id,
                    node_number,
                    service_number: 0,
                },
                vec![local::Action::AdminEndpoint].into(),
            );

            finals.insert(
                &format!("ipn:{allocator_id}.{node_number}.*")
                    .parse()
                    .unwrap(),
            );
        }

        if let Some(node_name) = &config.node_ids.dtn {
            // Add the Admin Endpoint EID itself
            actions.insert(
                Eid::Dtn {
                    node_name: node_name.clone(),
                    demux: "".into(),
                },
                vec![local::Action::AdminEndpoint].into(),
            );

            finals.insert(&format!("dtn://{node_name}/**").parse().unwrap());
        }

        Self { actions, finals }
    }
}

impl Rib {
    async fn add_local(&self, eid: Eid, action: Action) {
        info!("Adding local route {eid} => {action}");

        match self
            .inner
            .write()
            .trace_expect("Failed to lock mutex")
            .locals
            .actions
            .entry(eid.clone())
        {
            std::collections::hash_map::Entry::Occupied(mut occupied_entry) => {
                occupied_entry.get_mut().push(action);
            }
            std::collections::hash_map::Entry::Vacant(vacant_entry) => {
                vacant_entry.insert(vec![action].into());
            }
        }
    }

    pub async fn add_forward(&self, eid: Eid, peer_id: u32) {
        self.add_local(eid, Action::Forward(peer_id)).await

        // TODO: Re-evaluate NoRoute and Pending bundles
    }

    pub async fn add_service(&self, eid: Eid, service: Arc<service_registry::Service>) {
        self.add_local(eid, Action::Local(service)).await
    }

    fn remove_local(&self, eid: &Eid, mut f: impl FnMut(&Action) -> bool) -> bool {
        self.inner
            .write()
            .trace_expect("Failed to lock mutex")
            .locals
            .actions
            .get_mut(eid)
            .map(|h| {
                let mut removed = false;
                h.retain(|a| {
                    if f(a) {
                        info!("Removed route {eid} => {a}");
                        removed = true;
                        false
                    } else {
                        true
                    }
                });
                removed
            })
            .unwrap_or(false)
    }

    pub fn remove_forward(&self, eid: &Eid, peer_id: u32) -> bool {
        if !self.remove_local(eid, |action| match action {
            Action::Forward(p) => &peer_id == p,
            _ => false,
        }) {
            return false;
        }

        // TODO: Re-evaluate NoRoute and Pending bundles

        true
    }

    pub fn remove_service(&self, eid: &Eid, service: &service_registry::Service) -> bool {
        self.remove_local(eid, |action| {
            if let Action::Local(svc) = action {
                svc.as_ref() == service
            } else {
                false
            }
        })
    }
}
