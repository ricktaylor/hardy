use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Action {
    AdminEndpoint,                           // Deliver to the admin endpoint
    Local(Arc<services::registry::Service>), // Deliver to local service
    Forward(Arc<cla::entry::ClaEntry>),      // Forward via CLA
}

impl PartialOrd for Action {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Action {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
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
            Action::Local(service) => write!(f, "local service {}", &service.service_id),
            Action::Forward(cla_entry) => write!(f, "CLA {}", cla_entry.name),
        }
    }
}

pub struct LocalInner {
    pub actions: BTreeMap<EidPattern, BTreeSet<local::Action>>,
    pub finals: BTreeSet<EidPattern>,
}

impl LocalInner {
    pub fn new(node_ids: &node_ids::NodeIds) -> Self {
        let mut actions = BTreeMap::new();
        let mut finals = BTreeSet::new();

        // Add localnode admin endpoint
        actions.insert(
            NodeId::LocalNode.into(),
            [local::Action::AdminEndpoint].into(),
        );

        // Drop LocalNode services
        finals.insert(NodeId::LocalNode.into());

        if let Some(node_id) = &node_ids.ipn {
            // Add the Admin Endpoint EID itself (exact match, not wildcard)
            // Convert to Eid first to get ipn:N.0, then to EidPattern for exact match
            let admin_eid: Eid = (*node_id).into();
            actions.insert(admin_eid.into(), [local::Action::AdminEndpoint].into());
        }

        if let Some(node_name) = &node_ids.dtn {
            // Add the Admin Endpoint EID itself (exact match, not wildcard)
            let admin_eid: Eid = node_name.clone().into();
            actions.insert(admin_eid.into(), [local::Action::AdminEndpoint].into());
        }

        Self { actions, finals }
    }
}

impl Rib {
    // TODO: Add batched variants of add_local/remove_local that take a slice,
    // acquire the write lock once, and call notify_updated() once per batch.
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

    /// Add a forward route for a CLA.
    /// The NodeId is converted to a wildcard pattern (e.g., ipn:1.* for all services).
    pub async fn add_forward(&self, node_id: NodeId, cla_entry: Arc<cla::entry::ClaEntry>) -> bool {
        let pattern: EidPattern = node_id.into();
        self.add_local(pattern, Action::Forward(cla_entry)).await
    }

    /// Add a service route for a local service.
    /// The Eid is converted to an exact pattern.
    pub async fn add_service(&self, eid: Eid, service: Arc<services::registry::Service>) -> bool {
        let pattern: EidPattern = eid.into();
        self.add_local(pattern, Action::Local(service)).await
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

    /// Remove a forward route for a CLA.
    pub async fn remove_forward(&self, node_id: NodeId, cla_entry: &cla::entry::ClaEntry) -> bool {
        let pattern: EidPattern = node_id.into();
        if !self.remove_local(
            &pattern,
            |action| matches!(action, Action::Forward(e) if e.as_ref() == cla_entry),
        ) {
            return false;
        }

        self.notify_updated().await;
        true
    }

    /// Remove a service route for a local service.
    pub async fn remove_service(&self, eid: &Eid, service: &services::registry::Service) -> bool {
        let pattern: EidPattern = eid.clone().into();
        if !self.remove_local(
            &pattern,
            |action| matches!(action, Action::Local(svc) if svc.as_ref() == service),
        ) {
            return false;
        }
        self.notify_updated().await;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::make_cla_entry;
    use super::*;

    #[test]
    #[allow(clippy::mutable_key_type)]
    fn test_local_action_sort() {
        // AdminEndpoint < Local < Forward
        let admin = Action::AdminEndpoint;
        let forward_1 = Action::Forward(make_cla_entry("cla-a"));
        let forward_2 = Action::Forward(make_cla_entry("cla-b"));

        assert!(admin < forward_1);
        assert!(forward_1 < forward_2);

        // Verify BTreeSet ordering
        let mut set = BTreeSet::new();
        set.insert(forward_2.clone());
        set.insert(admin.clone());
        set.insert(forward_1.clone());

        let sorted: Vec<_> = set.into_iter().collect();
        assert_eq!(sorted[0], admin);
        assert_eq!(sorted[1], forward_1);
        assert_eq!(sorted[2], forward_2);
    }

    #[test]
    #[allow(clippy::mutable_key_type)]
    fn test_implicit_routes() {
        use hardy_bpv7::eid::IpnNodeId;

        let node_ids = node_ids::NodeIds {
            ipn: Some(IpnNodeId {
                allocator_id: 0,
                node_number: 1,
            }),
            dtn: None,
        };

        let inner = LocalInner::new(&node_ids);

        // Should have admin endpoint for LocalNode
        let local_node_pattern: EidPattern = hardy_bpv7::eid::NodeId::LocalNode.into();
        assert!(inner.actions.contains_key(&local_node_pattern));
        let actions = inner.actions.get(&local_node_pattern).unwrap();
        assert!(actions.contains(&Action::AdminEndpoint));

        // Should have admin endpoint for the IPN node's admin EID (ipn:0.1.0)
        let admin_eid: hardy_bpv7::eid::Eid = node_ids.ipn.unwrap().into();
        let admin_pattern: EidPattern = admin_eid.into();
        assert!(inner.actions.contains_key(&admin_pattern));

        // LocalNode should be in finals (drop unregistered services)
        assert!(inner.finals.contains(&local_node_pattern));
    }

    #[test]
    fn test_local_ephemeral() {
        // A bundle destined for a known-local EID (matches a final pattern)
        // but with no registered service should be dropped with
        // DestinationEndpointIDUnavailable.
        // The finals check happens in find_recurse (via find()), not find_local().
        let rib = super::super::tests::make_rib();

        // Add a finals entry so the RIB knows ipn:0.1.* is our node's address space
        {
            let mut inner = rib.inner.write();
            let pattern: EidPattern = rib.node_ids.ipn.unwrap().into();
            inner.locals.finals.insert(pattern);
        }

        // ipn:0.1.99 is under our node but no service is registered
        let mut bundle = bundle::Bundle {
            bundle: hardy_bpv7::bundle::Bundle {
                id: hardy_bpv7::bundle::Id {
                    source: "ipn:0.99.1".parse().unwrap(),
                    timestamp: hardy_bpv7::creation_timestamp::CreationTimestamp::now(),
                    fragment_info: None,
                },
                flags: Default::default(),
                crc_type: Default::default(),
                destination: "ipn:0.1.99".parse().unwrap(),
                report_to: Default::default(),
                lifetime: core::time::Duration::from_secs(3600),
                previous_node: None,
                age: None,
                hop_count: None,
                blocks: Default::default(),
            },
            metadata: Default::default(),
        };

        let result = rib.find(&mut bundle);
        assert!(matches!(
            result,
            Some(super::super::FindResult::Drop(Some(
                hardy_bpv7::status_report::ReasonCode::DestinationEndpointIDUnavailable
            )))
        ));
    }
}
