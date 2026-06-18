use core::hash::BuildHasher;

use futures::{FutureExt, select_biased};
use hardy_async::sync::RwLock;
use hardy_bpv7::{eid::Eid, status_report::ReasonCode};
use hardy_eid_patterns::EidPattern;
use tracing::{debug, info};

#[cfg(feature = "instrument")]
use tracing::instrument;

use super::action::{Action, InternalAction, RouteAction};
use super::agent;
use super::table::{self, Entry, RouteTable};
use super::{Error, Result, RoutingAgent};
use crate::{Arc, HashMap, HashSet, hash_map};
use crate::{bundle, cla, dispatcher, node_ids, services, storage};

#[derive(Debug)]
pub enum FindResult {
    AdminEndpoint,
    Deliver(Arc<services::registry::Service>),
    Forward(u32),
    Drop(Option<ReasonCode>),
}

struct RibInner {
    table: RouteTable,
    address_types: HashMap<cla::ClaAddressType, Arc<cla::registry::Cla>>,
}

pub struct Rib {
    inner: RwLock<RibInner>,
    // Routing agent tracking: spin::Mutex for O(1) HashMap operations
    agents: hardy_async::sync::spin::Mutex<HashMap<String, Arc<agent::Agent>>>,
    node_ids: Arc<node_ids::NodeIds>,
    // Fixed per-instance seed for deterministic ECMP peer selection.
    // Random across BPA instances (unpredictable), but consistent within
    // an instance so the same bundle always selects the same peer.
    ecmp_hash_state: foldhash::quality::RandomState,
    pub(crate) tasks: hardy_async::TaskPool,
    poll_waiting_notify: Arc<hardy_async::Notify>,
    store: Arc<storage::Store>,
    // The priority for services - default 1
    service_priority: u32,
}

pub(crate) struct RibBuilder {
    agents: Vec<(String, Arc<dyn RoutingAgent>)>,
    service_priority: u32,
}

impl RibBuilder {
    pub fn new() -> Self {
        Self {
            agents: Vec::new(),
            service_priority: 1,
        }
    }

    pub fn insert(&mut self, name: String, agent: Arc<dyn RoutingAgent>) {
        self.agents.push((name, agent));
    }

    pub fn service_priority(&mut self, priority: u32) {
        self.service_priority = priority;
    }

    pub async fn build(
        self,
        node_ids: Arc<node_ids::NodeIds>,
        store: Arc<storage::Store>,
    ) -> Result<Arc<Rib>> {
        let rib = Arc::new(Rib::new(node_ids, store, self.service_priority));
        for (name, agent) in self.agents {
            rib.register_agent(name, agent).await?;
        }
        Ok(rib)
    }
}

impl Rib {
    const FORWARDS_NAME: &str = "neighbours";
    const SERVICES_NAME: &str = "services";

    fn new(
        node_ids: Arc<node_ids::NodeIds>,
        store: Arc<storage::store::Store>,
        service_priority: u32,
    ) -> Self {
        Self {
            inner: RwLock::new(RibInner {
                table: RouteTable::new(node_ids.clone()),
                address_types: HashMap::new(),
            }),
            agents: Default::default(),
            node_ids,
            ecmp_hash_state: foldhash::quality::RandomState::default(),
            tasks: hardy_async::TaskPool::new(),
            poll_waiting_notify: Arc::new(hardy_async::Notify::new()),
            store,
            service_priority,
        }
    }

    pub(crate) fn start(self: &Arc<Self>, dispatcher: Arc<dispatcher::Dispatcher>) {
        let cancel_token = self.tasks.cancel_token().clone();
        let rib = self.clone();
        hardy_async::spawn!(self.tasks, "poll_waiting_task", async move {
            loop {
                select_biased! {
                    _ = cancel_token.cancelled().fuse() => {
                        break;
                    }
                    _ = rib.poll_waiting_notify.notified().fuse() => {
                        dispatcher.poll_waiting(cancel_token.clone()).await;
                    },
                }
            }
            debug!("Poll waiting task complete");
        });

        self.poll_waiting_notify.notify_one();
    }

    pub async fn shutdown(&self) {
        self.tasks.shutdown().await;
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    pub fn find(&self, bundle: &mut bundle::Bundle) -> Option<FindResult> {
        let inner = self.inner.read();

        let result =
            inner
                .table
                .find_recurse(&bundle.bundle.destination, true, &mut HashSet::new())?;

        let previous;
        let result = if matches!(result, table::FindResult::Reflect) {
            previous = bundle
                .previous_node()
                .unwrap_or_else(|| bundle.bundle.id.source.clone());
            inner
                .table
                .find_recurse(&previous, false, &mut HashSet::new())?
        } else {
            result
        };

        match result {
            table::FindResult::AdminEndpoint => Some(FindResult::AdminEndpoint),
            table::FindResult::Deliver(service) => Some(FindResult::Deliver(service)),
            table::FindResult::Drop(reason) => Some(FindResult::Drop(reason)),
            table::FindResult::Forward(peers) => {
                self.select_peer(peers, &bundle.bundle, &mut bundle.metadata)
            }
            table::FindResult::Reflect => None,
        }
    }

    pub fn find_service(&self, to: &Eid) -> Option<Arc<services::registry::Service>> {
        let inner = self.inner.read();
        inner.table.find_service(to)
    }

    fn find_peers(&self, to: &Eid) -> Option<HashSet<u32>> {
        let inner = self.inner.read();
        inner.table.find_peers(to)
    }

    fn select_peer(
        &self,
        mut peers: Vec<(u32, &Eid)>,
        bundle: &hardy_bpv7::bundle::Bundle,
        metadata: &mut bundle::BundleMetadata,
    ) -> Option<FindResult> {
        if peers.is_empty() {
            debug_assert!(false, "Empty Forward result from find_recurse");
            return None;
        }

        trace!(peers = ?peers, "Forward to CLA peers");

        let idx = if peers.len() > 1 {
            (self.ecmp_hash_state.hash_one((
                &bundle.id.source,
                &bundle.destination,
                &metadata.writable.flow_label,
            )) % (peers.len() as u64)) as usize
        } else {
            0
        };
        let (peer, next_hop) = peers.swap_remove(idx);
        metadata.read_only.next_hop = Some(next_hop.clone());
        Some(FindResult::Forward(peer))
    }

    pub(crate) async fn add(
        &self,
        pattern: EidPattern,
        source: String,
        action: Action,
        priority: u32,
    ) -> bool {
        let vias = {
            let entry = Entry {
                action: action.clone(),
                source: source.clone(),
            };

            let mut inner = self.inner.write();
            if !inner.table.insert(pattern.clone(), entry, priority) {
                return false;
            }

            debug!("Adding route {pattern} => {action}, priority {priority}, source '{source}'");
            metrics::gauge!("bpa.rib.entries", "source" => source).increment(1.0);

            inner.table.impacted_vias(&pattern, priority)
        };

        let changed = match action {
            Action::Internal(InternalAction::AdminEndpoint) => false,
            Action::Internal(InternalAction::Local(_))
            | Action::Internal(InternalAction::Forward(_)) => true,
            Action::Route(_) => {
                let mut changed = false;
                for v in vias {
                    if let Some(peers) = self.find_peers(&v)
                        && self.reset_peer_queues(peers).await
                    {
                        changed = true;
                    }
                }
                changed
            }
        };
        if changed {
            self.notify_updated().await;
        }
        true
    }

    pub(crate) async fn remove(
        &self,
        pattern: &EidPattern,
        source: &str,
        action: Action,
        priority: u32,
    ) -> bool {
        {
            let entry = Entry {
                action: action.clone(),
                source: source.to_string(),
            };
            let mut inner = self.inner.write();
            if !inner.table.remove(pattern, &entry, priority) {
                return false;
            }
        }

        debug!("Removed route {pattern} => {action}, priority {priority}, source '{source}'");
        metrics::gauge!("bpa.rib.entries", "source" => source.to_string()).decrement(1.0);

        match action {
            Action::Route(RouteAction::Via(ref to)) => {
                if let Some(peers) = self.find_peers(to)
                    && self.reset_peer_queues(peers).await
                {
                    self.notify_updated().await;
                }
            }
            Action::Internal(InternalAction::Forward(peer))
                if self.store.reset_peer_queue(peer).await =>
            {
                self.notify_updated().await;
            }
            Action::Internal(InternalAction::Local(_)) => {
                self.notify_updated().await;
            }
            _ => {}
        }
        true
    }

    pub async fn remove_by_source(&self, source: &str) {
        let (vias, forward_peers, has_local, removed_count) = {
            let mut inner = self.inner.write();
            inner.table.remove_by_source(source)
        };

        if removed_count == 0 {
            return;
        }

        debug!("Removed all routes from source '{source}'");
        metrics::gauge!("bpa.rib.entries", "source" => source.to_string())
            .decrement(removed_count as f64);

        let mut changed = has_local;
        for v in vias {
            if let Some(peers) = self.find_peers(&v)
                && self.reset_peer_queues(peers).await
            {
                changed = true;
            }
        }
        for peer in forward_peers {
            if self.store.reset_peer_queue(peer).await {
                changed = true;
            }
        }
        if changed {
            self.notify_updated().await;
        }
    }

    pub async fn add_forward(&self, node_id: NodeId, peer: u32) -> bool {
        let pattern: EidPattern = node_id.into();
        self.add(
            pattern,
            Self::FORWARDS_NAME.into(),
            Action::Internal(InternalAction::Forward(peer)),
            0,
        )
        .await
    }

    pub async fn remove_forward(&self, node_id: NodeId, peer: u32) -> bool {
        let pattern: EidPattern = node_id.into();
        self.remove(
            &pattern,
            Self::FORWARDS_NAME,
            Action::Internal(InternalAction::Forward(peer)),
            0,
        )
        .await
    }

    pub async fn add_service(&self, eid: Eid, service: Arc<services::registry::Service>) -> bool {
        self.add(
            eid.into(),
            Self::SERVICES_NAME.into(),
            Action::Internal(InternalAction::Local(service)),
            self.service_priority,
        )
        .await
    }

    pub async fn remove_service(
        &self,
        eid: &Eid,
        service: Arc<services::registry::Service>,
    ) -> bool {
        let pattern: EidPattern = eid.clone().into();
        self.remove(
            &pattern,
            Self::SERVICES_NAME,
            Action::Internal(InternalAction::Local(service)),
            self.service_priority,
        )
        .await
    }

    pub(crate) async fn register_agent(
        self: &Arc<Self>,
        name: String,
        agent: Arc<dyn RoutingAgent>,
    ) -> Result<Vec<NodeId>> {
        let agent = {
            let mut agents = self.agents.lock();
            let hash_map::Entry::Vacant(e) = agents.entry(name.clone()) else {
                return Err(Error::AlreadyExists(name));
            };

            info!("Registered new routing agent: {name}");

            e.insert(Arc::new(agent::Agent { agent, name })).clone()
        };

        metrics::gauge!("bpa.rib.agents").increment(1.0);

        let node_ids: Vec<NodeId> = (&*self.node_ids).into();

        agent
            .agent
            .on_register(
                Box::new(agent::sink::Sink::new(Arc::downgrade(&agent), self.clone())),
                &node_ids,
            )
            .await;

        Ok(node_ids)
    }

    pub(crate) async fn unregister_agent(&self, agent: Arc<agent::Agent>) {
        let agent = self.agents.lock().remove(&agent.name);

        if let Some(agent) = agent {
            metrics::gauge!("bpa.rib.agents").decrement(1.0);
            agent.agent.on_unregister().await;
            self.remove_by_source(&agent.name).await;
            info!("Unregistered routing agent: {}", agent.name);
        }
    }

    pub(crate) async fn shutdown_agents(&self) {
        let agents = self
            .agents
            .lock()
            .drain()
            .map(|(_, v)| v)
            .collect::<Vec<_>>();

        if !agents.is_empty() {
            metrics::gauge!("bpa.rib.agents").decrement(agents.len() as f64);
        }

        for agent in agents {
            agent.agent.on_unregister().await;
            self.remove_by_source(&agent.name).await;
            info!("Unregistered routing agent: {}", agent.name);
        }
    }

    pub fn add_address_type(
        &self,
        address_type: cla::ClaAddressType,
        cla: Arc<cla::registry::Cla>,
    ) {
        self.inner.write().address_types.insert(address_type, cla);
    }

    pub fn remove_address_type(&self, address_type: &cla::ClaAddressType) {
        self.inner.write().address_types.remove(address_type);
    }

    // -- Internal helpers ----------------------------------------------------

    async fn notify_updated(&self) {
        self.poll_waiting_notify.notify_waiters();
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
    use super::*;
    use hardy_bpv7::eid::IpnNodeId;

    fn make_rib() -> Arc<Rib> {
        let node_ids = Arc::new(node_ids::NodeIds {
            ipn: Some(IpnNodeId {
                allocator_id: 0,
                node_number: 1,
            }),
            dtn: None,
        });

        let store = Arc::new(storage::Store::new(
            core::num::NonZeroUsize::new(16).unwrap(),
            Arc::new(storage::MetadataMemStorage::new(&Default::default())),
            Arc::new(storage::BundleMemStorage::new(&Default::default())),
        ));

        Arc::new(Rib::new(node_ids, store, 1))
    }

    fn add_route(rib: &Rib, pattern: &str, source: &str, action: Action, priority: u32) {
        let pattern: EidPattern = pattern.parse().unwrap();
        let entry = Entry {
            action,
            source: source.to_string(),
        };
        let mut inner = rib.inner.write();
        inner.table.insert(pattern, entry, priority);
    }

    fn add_local_forward(rib: &Rib, node_id: NodeId, peer: u32) {
        let pattern: EidPattern = node_id.into();
        add_route(
            rib,
            &pattern.to_string(),
            "forward",
            Action::Internal(InternalAction::Forward(peer)),
            0,
        )
    }

    fn make_bundle(destination: &str) -> bundle::Bundle {
        bundle::Bundle {
            bundle: hardy_bpv7::bundle::Bundle {
                id: hardy_bpv7::bundle::Id {
                    source: "ipn:0.99.1".parse().unwrap(),
                    timestamp: hardy_bpv7::creation_timestamp::CreationTimestamp::now(),
                    fragment_info: None,
                },
                flags: Default::default(),
                crc_type: Default::default(),
                destination: destination.parse().unwrap(),
                report_to: Default::default(),
                lifetime: core::time::Duration::from_secs(3600),
                previous_node: None,
                age: None,
                hop_count: None,
                blocks: Default::default(),
            },
            metadata: Default::default(),
        }
    }

    fn ipn_node(n: u32) -> NodeId {
        NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: n,
        })
    }

    #[test]
    fn test_exact_match() {
        let rib = make_rib();
        add_local_forward(&rib, ipn_node(2), 42);

        let mut bundle = make_bundle("ipn:0.2.1");
        let result = rib.find(&mut bundle);
        assert!(matches!(result, Some(FindResult::Forward(42))));
    }

    #[test]
    fn test_default_route() {
        let rib = make_rib();
        add_route(
            &rib,
            "ipn:0.50.*",
            "default",
            Action::Route(RouteAction::Via("ipn:0.10.0".parse().unwrap())),
            1000,
        );
        add_local_forward(&rib, ipn_node(10), 99);

        let mut bundle = make_bundle("ipn:0.50.1");
        let result = rib.find(&mut bundle);
        assert!(matches!(result, Some(FindResult::Forward(99))));
    }

    #[test]
    fn test_no_route() {
        let rib = make_rib();
        let mut bundle = make_bundle("ipn:0.50.1");
        let result = rib.find(&mut bundle);
        assert!(result.is_none());
    }

    #[test]
    fn test_recursion_loop() {
        let rib = make_rib();
        add_route(
            &rib,
            "ipn:0.2.*",
            "loop",
            Action::Route(RouteAction::Via("ipn:0.3.0".parse().unwrap())),
            10,
        );
        add_route(
            &rib,
            "ipn:0.3.*",
            "loop",
            Action::Route(RouteAction::Via("ipn:0.2.0".parse().unwrap())),
            10,
        );

        let mut bundle = make_bundle("ipn:0.2.1");
        let result = rib.find(&mut bundle);
        assert!(
            result.is_none(),
            "Recursive route should return None (wait), not Drop"
        );
    }

    #[test]
    fn test_reflection() {
        let rib = make_rib();
        add_route(
            &rib,
            "ipn:0.5.*",
            "reflect",
            Action::Route(RouteAction::Reflect),
            10,
        );
        add_local_forward(&rib, ipn_node(4), 77);

        let mut bundle = make_bundle("ipn:0.5.1");
        bundle.bundle.previous_node = Some("ipn:0.4.0".parse().unwrap());
        let result = rib.find(&mut bundle);
        assert!(matches!(result, Some(FindResult::Forward(77))));
    }

    #[test]
    fn test_reflection_no_double() {
        let rib = make_rib();
        add_route(
            &rib,
            "ipn:0.5.*",
            "r",
            Action::Route(RouteAction::Reflect),
            10,
        );
        add_route(
            &rib,
            "ipn:0.4.*",
            "r",
            Action::Route(RouteAction::Reflect),
            10,
        );

        let mut bundle = make_bundle("ipn:0.5.1");
        bundle.bundle.previous_node = Some("ipn:0.4.0".parse().unwrap());
        let result = rib.find(&mut bundle);
        assert!(result.is_none());
    }

    #[test]
    fn test_ecmp_hashing() {
        let rib = make_rib();
        add_route(
            &rib,
            "ipn:0.50.*",
            "ecmp_a",
            Action::Route(RouteAction::Via("ipn:0.10.0".parse().unwrap())),
            10,
        );
        add_route(
            &rib,
            "ipn:0.50.*",
            "ecmp_b",
            Action::Route(RouteAction::Via("ipn:0.11.0".parse().unwrap())),
            10,
        );
        add_local_forward(&rib, ipn_node(10), 10);
        add_local_forward(&rib, ipn_node(11), 11);

        let mut bundle = make_bundle("ipn:0.50.1");
        let result1 = rib.find(&mut bundle);
        let peer1 = match result1 {
            Some(FindResult::Forward(p)) => p,
            other => panic!("Expected Forward, got {other:?}"),
        };

        let mut bundle2 = make_bundle("ipn:0.50.1");
        bundle2.bundle.id = bundle.bundle.id.clone();
        let result2 = rib.find(&mut bundle2);
        let peer2 = match result2 {
            Some(FindResult::Forward(p)) => p,
            other => panic!("Expected Forward, got {other:?}"),
        };

        assert_eq!(peer1, peer2, "ECMP selection must be deterministic");
        assert!(
            peer1 == 10 || peer1 == 11,
            "Peer must be one of the ECMP targets, got {peer1}"
        );
    }

    #[test]
    fn test_admin_endpoint_lookup() {
        let rib = make_rib();
        let mut bundle = make_bundle("ipn:0.1.0");
        let result = rib.find(&mut bundle);
        assert!(
            matches!(result, Some(FindResult::AdminEndpoint)),
            "Admin EID should resolve to AdminEndpoint, got {result:?}"
        );
    }

    #[test]
    fn test_unregistered_local_waits() {
        let rib = make_rib();
        let mut bundle = make_bundle("ipn:0.1.99");
        let result = rib.find(&mut bundle);
        assert!(
            result.is_none(),
            "Unregistered local service should wait (no route), got {result:?}"
        );
    }

    #[test]
    fn test_concrete_service_matches() {
        let rib = make_rib();
        add_route(
            &rib,
            "ipn:0.1.42",
            "services",
            Action::Internal(InternalAction::Local(Arc::new(
                services::registry::Service {
                    service: services::registry::ServiceImpl::LowLevel(Arc::new(
                        crate::services::tests::NullService,
                    )),
                    service_id: hardy_bpv7::eid::Service::Ipn(42),
                },
            ))),
            1,
        );

        let mut bundle = make_bundle("ipn:0.1.42");
        let result = rib.find(&mut bundle);
        assert!(
            matches!(result, Some(FindResult::Deliver(_))),
            "got {result:?}"
        );
    }

    #[test]
    fn test_concrete_service_ignores_remote_eid() {
        let rib = make_rib();
        add_route(
            &rib,
            "ipn:0.1.42",
            "services",
            Action::Internal(InternalAction::Local(Arc::new(
                services::registry::Service {
                    service: services::registry::ServiceImpl::LowLevel(Arc::new(
                        crate::services::tests::NullService,
                    )),
                    service_id: hardy_bpv7::eid::Service::Ipn(42),
                },
            ))),
            1,
        );

        let mut bundle = make_bundle("ipn:0.2.42");
        let result = rib.find(&mut bundle);
        assert!(
            result.is_none(),
            "Remote EID should not match local service route, got {result:?}"
        );
    }

    #[test]
    fn test_admin_endpoint_matches_concrete() {
        let rib = make_rib();
        let mut bundle = make_bundle("ipn:0.1.0");
        let result = rib.find(&mut bundle);
        assert!(
            matches!(result, Some(FindResult::AdminEndpoint)),
            "got {result:?}"
        );
    }

    #[test]
    fn test_explicit_drop_overrides_wait() {
        let rib = make_rib();
        add_route(
            &rib,
            "ipn:0.1.*",
            "policy",
            Action::Route(RouteAction::Drop(Some(
                ReasonCode::DestinationEndpointIDUnavailable,
            ))),
            10,
        );

        let mut bundle = make_bundle("ipn:0.1.99");
        let result = rib.find(&mut bundle);
        assert!(
            matches!(
                result,
                Some(FindResult::Drop(Some(
                    ReasonCode::DestinationEndpointIDUnavailable
                )))
            ),
            "Explicit drop rule should override default wait, got {result:?}"
        );
    }

    #[tokio::test]
    async fn test_reject_null_next_hop() {
        let rib = make_rib();
        let result = rib
            .add(
                "ipn:0.2.*".parse().unwrap(),
                "test".into(),
                Action::Route(RouteAction::Via(Eid::Null)),
                10,
            )
            .await;
        assert!(!result, "Via null endpoint should be rejected");
    }

    #[tokio::test]
    async fn test_reject_via_own_node() {
        let rib = make_rib();
        let result = rib
            .add(
                "ipn:0.99.*".parse().unwrap(),
                "test".into(),
                Action::Route(RouteAction::Via("ipn:0.1.0".parse().unwrap())),
                10,
            )
            .await;
        assert!(!result, "Via own node should be rejected");
    }

    #[tokio::test]
    async fn test_allow_default_route() {
        let rib = make_rib();
        let result = rib
            .add(
                "*:**".parse().unwrap(),
                "test".into(),
                Action::Route(RouteAction::Via("ipn:0.2.0".parse().unwrap())),
                10,
            )
            .await;
        assert!(result, "Default route should be accepted");
    }
}
