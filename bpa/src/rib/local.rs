use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Action {
    AdminEndpoint,                                 // Deliver to the admin endpoint
    Local(Option<Arc<service_registry::Service>>), // Deliver to local service
    Forward(u32),                                  // Forward to a cla peer
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
            Eid::LocalNode { service_number: 0 },
            [local::Action::AdminEndpoint].into(),
        );

        // Wait for well-known services
        // TODO: Drive this from a services file...

        // Drop LocalNode services
        finals.insert(
            "ipn:!.*"
                .parse()
                .trace_expect("Failed to parse hard-coded pattern item"),
        );

        if let Some((allocator_id, node_number)) = config.node_ids.ipn {
            // Add the Admin Endpoint EID itself
            actions.insert(
                Eid::Ipn {
                    allocator_id,
                    node_number,
                    service_number: 0,
                },
                [local::Action::AdminEndpoint].into(),
            );

            // Wait for well-known services
            // TODO: Drive this from a services file...

            // Drop ephemeral services
            finals.insert(
                format!("ipn:{allocator_id}.{node_number}.*")
                    .parse()
                    .trace_expect(
                        "Failed to parse pattern item: 'ipn:{allocator_id}.{node_number}.*'",
                    ),
            );
        }

        if let Some(node_name) = &config.node_ids.dtn {
            // Add the Admin Endpoint EID itself
            actions.insert(
                Eid::Dtn {
                    node_name: node_name.clone(),
                    demux: "".into(),
                },
                [local::Action::AdminEndpoint].into(),
            );

            // Wait for well-known services
            // TODO: Drive this from a services file...

            // Drop ephemeral services
            finals.insert(
                format!("dtn://{node_name}/**")
                    .parse()
                    .trace_expect("Failed to parse pattern item: 'dtn://{node_name}/auto/**'"),
            );
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

    pub async fn add_forward(&self, eid: Eid, peer: u32) -> bool {
        self.add_local(eid, Action::Forward(peer)).await
    }

    pub async fn add_service(&self, eid: Eid, service: Arc<service_registry::Service>) -> bool {
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

    pub async fn remove_forward(&self, eid: &Eid, peer: u32) -> bool {
        if !self.remove_local(
            eid,
            |action| matches!(action, Action::Forward(p) if &peer == p),
        ) {
            return false;
        }

        match self.store.reset_peer_queue(peer).await {
            Ok(true) => {
                self.notify_updated().await;
            }
            Ok(false) => {}
            Err(e) => {
                error!("Failed to reset peer queue: {e}");
            }
        }
        true
    }

    pub fn remove_service(&self, eid: &Eid, service: &service_registry::Service) -> bool {
        self.remove_local(
            eid,
            |action| matches!(action, Action::Local(Some(svc)) if svc.as_ref() == service),
        )
    }
}
