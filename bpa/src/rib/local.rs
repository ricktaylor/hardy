use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Action {
    AdminEndpoint,                                   // Deliver to the admin endpoint
    Local(Option<Arc<services::registry::Service>>), // Deliver to local service
    Forward(u32),                                    // Forward to a cla peer
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
            (Action::AdminEndpoint, _) => std::cmp::Ordering::Less,
            (Action::Local(_), Action::AdminEndpoint) => std::cmp::Ordering::Greater,
            (Action::Local(lhs), Action::Local(rhs)) => lhs.cmp(rhs),
            (Action::Local(_), Action::Forward(..)) => std::cmp::Ordering::Less,
            (Action::Forward(lhs), Action::Forward(rhs)) => lhs.cmp(rhs),
            (Action::Forward(_), _) => std::cmp::Ordering::Greater,
        }
    }
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Action::AdminEndpoint => write!(f, "administrative endpoint"),
            Action::Local(Some(service)) => write!(f, "local service {}", &service.service_id),
            Action::Local(None) => write!(f, "well-known service"),
            Action::Forward(peer) => {
                write!(f, "CLA peer {peer}")
            }
        }
    }
}

pub struct LocalInner {
    pub actions: HashMap<Eid, BTreeSet<local::Action>>,
    pub finals: BTreeSet<EidPattern>,
}

impl LocalInner {
    pub fn new(config: &config::Config) -> Self {
        let mut actions = HashMap::new();
        let mut finals = BTreeSet::new();

        // Add localnode admin endpoint
        actions.insert(
            NodeId::LocalNode.into(),
            [local::Action::AdminEndpoint].into(),
        );

        // Wait for well-known services
        // TODO: Drive this from a services file...

        // Drop LocalNode services
        finals.insert(NodeId::LocalNode.into());

        if let Some(node_id) = &config.node_ids.ipn {
            // Add the Admin Endpoint EID itself
            actions.insert(
                node_id.clone().into(),
                [local::Action::AdminEndpoint].into(),
            );

            // Wait for well-known services
            // TODO: Drive this from a services file...

            // Drop ephemeral services
            finals.insert(node_id.clone().into());
        }

        if let Some(node_name) = &config.node_ids.dtn {
            // Add the Admin Endpoint EID itself
            actions.insert(
                node_name.clone().into(),
                [local::Action::AdminEndpoint].into(),
            );

            // Wait for well-known services
            // TODO: Drive this from a services file...

            // Drop ephemeral services
            finals.insert(node_name.clone().into());
        }

        Self { actions, finals }
    }
}

impl Rib {
    async fn add_local(&self, eid: Eid, action: Action) -> bool {
        info!("Adding local route {eid} => {action}");

        if !match self
            .inner
            .write()
            .trace_expect("Failed to lock mutex")
            .locals
            .actions
            .entry(eid.clone())
        {
            std::collections::hash_map::Entry::Occupied(mut occupied_entry) => {
                occupied_entry.get_mut().insert(action)
            }
            std::collections::hash_map::Entry::Vacant(vacant_entry) => {
                vacant_entry.insert([action].into());
                true
            }
        } {
            return false;
        }

        self.notify_updated().await;
        true
    }

    pub async fn add_forward(&self, node_id: NodeId, peer: u32) -> bool {
        self.add_local(node_id.into(), Action::Forward(peer)).await
    }

    pub async fn add_service(&self, eid: Eid, service: Arc<services::registry::Service>) -> bool {
        self.add_local(eid, Action::Local(Some(service))).await
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

    pub async fn remove_forward(&self, node_id: NodeId, peer: u32) -> bool {
        if !self.remove_local(
            &node_id.into(),
            |action| matches!(action, Action::Forward(p) if &peer == p),
        ) {
            return false;
        }

        if self.store.reset_peer_queue(peer).await {
            self.notify_updated().await;
        }
        true
    }

    pub fn remove_service(&self, eid: &Eid, service: &services::registry::Service) -> bool {
        self.remove_local(
            eid,
            |action| matches!(action, Action::Local(Some(svc)) if svc.as_ref() == service),
        )
    }
}

#[cfg(test)]
mod tests {
    // use super::*;

    // // TODO: Implement test for 'Local Ephemeral' (Verify drop for known-local but unregistered service)
    // #[test]
    // fn test_local_ephemeral() {
    //     todo!("Verify drop for known-local but unregistered service");
    // }

    // // TODO: Implement test for 'Local Action Sort' (Verify Ord impl for local::Action)
    // #[test]
    // fn test_local_action_sort() {
    //     todo!("Verify Ord impl for local::Action");
    // }

    // // TODO: Implement test for 'Implicit Routes' (Verify default routes created on startup)
    // #[test]
    // fn test_implicit_routes() {
    //     todo!("Verify default routes created on startup");
    // }
}
