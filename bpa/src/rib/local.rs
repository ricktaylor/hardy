use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Action {
    AdminEndpoint,                                   // Deliver to the admin endpoint
    Local(Option<Arc<services::registry::Service>>), // Deliver to local service
    Forward(u32),                                    // Forward to a cla peer
}

impl PartialOrd for Action {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Action {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        // The order is critical, hence done long-hand
        match (self, other) {
            (Action::AdminEndpoint, Action::AdminEndpoint) => core::cmp::Ordering::Equal,
            (Action::AdminEndpoint, _) => core::cmp::Ordering::Less,
            (Action::Local(_), Action::AdminEndpoint) => core::cmp::Ordering::Greater,
            (Action::Local(lhs), Action::Local(rhs)) => lhs.cmp(rhs),
            (Action::Local(_), Action::Forward(..)) => core::cmp::Ordering::Less,
            (Action::Forward(lhs), Action::Forward(rhs)) => lhs.cmp(rhs),
            (Action::Forward(_), _) => core::cmp::Ordering::Greater,
        }
    }
}

impl core::fmt::Display for Action {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
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
    pub actions: BTreeMap<EidPattern, BTreeSet<local::Action>>,
    pub finals: BTreeSet<EidPattern>,
}

impl LocalInner {
    pub fn new(config: &config::Config) -> Self {
        let mut actions = BTreeMap::new();
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
            // Add the Admin Endpoint EID itself (exact match, not wildcard)
            // Convert to Eid first to get ipn:N.0, then to EidPattern for exact match
            let admin_eid: Eid = node_id.clone().into();
            actions.insert(admin_eid.into(), [local::Action::AdminEndpoint].into());

            // Wait for well-known services
            // TODO: Drive this from a services file...

            // Drop ephemeral services (wildcard pattern for all services on this node)
            // IpnNodeId -> EidPattern creates wildcard ipn:N.*
            finals.insert(node_id.clone().into());
        }

        if let Some(node_name) = &config.node_ids.dtn {
            // Add the Admin Endpoint EID itself (exact match, not wildcard)
            let admin_eid: Eid = node_name.clone().into();
            actions.insert(admin_eid.into(), [local::Action::AdminEndpoint].into());

            // Wait for well-known services
            // TODO: Drive this from a services file...

            // Drop ephemeral services (wildcard pattern for all services on this node)
            // DtnNodeId -> EidPattern creates wildcard dtn://node/**
            finals.insert(node_name.clone().into());
        }

        Self { actions, finals }
    }
}

impl Rib {
    async fn add_local(&self, pattern: EidPattern, action: Action) -> bool {
        debug!("Adding local route {pattern} => {action}");

        if !match self.inner.write().locals.actions.entry(pattern.clone()) {
            btree_map::Entry::Occupied(mut occupied_entry) => {
                occupied_entry.get_mut().insert(action)
            }
            btree_map::Entry::Vacant(vacant_entry) => {
                vacant_entry.insert([action].into());
                true
            }
        } {
            return false;
        }

        self.notify_updated().await;
        true
    }

    /// Add a forward route for a CLA peer.
    /// The NodeId is converted to a wildcard pattern (e.g., ipn:1.* for all services).
    pub async fn add_forward(&self, node_id: NodeId, peer: u32) -> bool {
        let pattern: EidPattern = node_id.into();
        self.add_local(pattern, Action::Forward(peer)).await
    }

    /// Add a service route for a local service.
    /// The Eid is converted to an exact pattern.
    pub async fn add_service(&self, eid: Eid, service: Arc<services::registry::Service>) -> bool {
        let pattern: EidPattern = eid.into();
        self.add_local(pattern, Action::Local(Some(service))).await
    }

    fn remove_local(&self, pattern: &EidPattern, mut f: impl FnMut(&Action) -> bool) -> bool {
        self.inner
            .write()
            .locals
            .actions
            .get_mut(pattern)
            .map(|h| {
                let mut removed = false;
                h.retain(|a| {
                    if f(a) {
                        debug!("Removed route {pattern} => {a}");
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

    /// Remove a forward route for a CLA peer.
    pub async fn remove_forward(&self, node_id: NodeId, peer: u32) -> bool {
        let pattern: EidPattern = node_id.into();
        if !self.remove_local(
            &pattern,
            |action| matches!(action, Action::Forward(p) if &peer == p),
        ) {
            return false;
        }

        if self.store.reset_peer_queue(peer).await {
            self.notify_updated().await;
        }
        true
    }

    /// Remove a service route for a local service.
    pub fn remove_service(&self, eid: &Eid, service: &services::registry::Service) -> bool {
        let pattern: EidPattern = eid.clone().into();
        self.remove_local(
            &pattern,
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
