use core::hash::BuildHasher;

use arc_swap::ArcSwap;
use foldhash::quality::RandomState;
use futures::{FutureExt, select_biased};
use hardy_async::{
    Notify, TaskPool,
    sync::{Mutex, spin},
};
use hardy_bpv7::{
    bundle::Bundle as Bpv7Bundle,
    eid::{Eid, NodeId},
    status_report::ReasonCode,
};
use hardy_eid_patterns::EidPattern;
use tracing::{debug, info, trace};

#[cfg(feature = "instrument")]
use tracing::instrument;

use super::{
    Error, Result, RoutingAgent,
    action::{Action, InternalAction, RouteAction},
    agent::sink::Sink,
    table::{Entry, LookupResult, RouteTable},
};
use crate::{
    Arc, HashMap, HashSet,
    bundle::{Bundle, BundleMetadata},
    cla::{ClaAddressType, registry::Cla},
    dispatcher::Dispatcher,
    hash_map::Entry as HashMapEntry,
    node_ids::NodeIds,
    services::registry::Service,
    storage::store::Store,
};

#[derive(Debug)]
pub enum DispatchAction {
    AdminEndpoint,
    Deliver(Arc<Service>),
    Forward(u32),
    Drop(Option<ReasonCode>),
}

pub struct Rib {
    snapshot: ArcSwap<RouteTable>,
    table: Mutex<RouteTable>,
    address_types: spin::Mutex<HashMap<ClaAddressType, Arc<Cla>>>,
    agents: spin::Mutex<HashMap<String, Arc<dyn RoutingAgent>>>,
    node_ids: Arc<NodeIds>,
    // Fixed per-instance seed for deterministic ECMP peer selection.
    // Random across BPA instances (unpredictable), but consistent within
    // an instance so the same bundle always selects the same peer.
    ecmp_hash_state: RandomState,
    pub(crate) tasks: TaskPool,
    poll_waiting_notify: Arc<Notify>,
    store: Arc<Store>,
    service_priority: u32,
}

pub struct RibBuilder {
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

    pub async fn build(self, node_ids: Arc<NodeIds>, store: Arc<Store>) -> Result<Arc<Rib>> {
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

    fn new(node_ids: Arc<NodeIds>, store: Arc<Store>, service_priority: u32) -> Self {
        let table = RouteTable::new(node_ids.clone());
        Self {
            snapshot: ArcSwap::from_pointee(table.clone()),
            table: Mutex::new(table),
            address_types: Default::default(),
            agents: Default::default(),
            node_ids,
            ecmp_hash_state: RandomState::default(),
            tasks: TaskPool::new(),
            poll_waiting_notify: Arc::new(Notify::new()),
            store,
            service_priority,
        }
    }

    pub(crate) fn start(self: &Arc<Self>, dispatcher: Arc<Dispatcher>) {
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
    pub fn find(&self, bundle: &mut Bundle) -> Option<DispatchAction> {
        let table = self.snapshot.load();

        let result = table.find_recurse(&bundle.bundle.destination, true, &mut HashSet::new())?;

        let previous;
        let result = if matches!(result, LookupResult::Reflect) {
            previous = bundle
                .previous_node()
                .unwrap_or_else(|| bundle.bundle.id.source.clone());
            table.find_recurse(&previous, false, &mut HashSet::new())?
        } else {
            result
        };

        match result {
            LookupResult::AdminEndpoint => Some(DispatchAction::AdminEndpoint),
            LookupResult::Deliver(service) => Some(DispatchAction::Deliver(service)),
            LookupResult::Drop(reason) => Some(DispatchAction::Drop(reason)),
            LookupResult::Forward(peer, next_hop) => {
                bundle.metadata.read_only.next_hop = Some(next_hop.clone());
                Some(DispatchAction::Forward(peer))
            }
            LookupResult::ForwardEcmp(peers) => {
                self.select_peer(peers, &bundle.bundle, &mut bundle.metadata)
            }
            LookupResult::Reflect => None,
        }
    }

    pub fn find_service(&self, to: &Eid) -> Option<Arc<Service>> {
        self.snapshot.load().find_service(to)
    }

    fn find_peers(&self, to: &Eid) -> Option<HashSet<u32>> {
        self.snapshot.load().find_peers(to)
    }

    fn select_peer(
        &self,
        mut peers: Vec<(u32, &Eid)>,
        bundle: &Bpv7Bundle,
        metadata: &mut BundleMetadata,
    ) -> Option<DispatchAction> {
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
        Some(DispatchAction::Forward(peer))
    }

    pub(crate) async fn add(
        &self,
        pattern: EidPattern,
        source: String,
        action: Action,
        priority: u32,
    ) -> Result<bool> {
        let pattern = self.expand_pattern(pattern);
        let action = self.expand_action(action);

        // Mutate the authoritative table in place, then publish a clone
        // to the snapshot for readers. The deep copy is acceptable here:
        // route mutations are management-plane, not per-bundle.
        let vias = {
            let mut table = self.table.lock();

            let entry = Entry {
                action: action.clone(),
                source: source.clone(),
            };
            if !table.insert(pattern.clone(), entry, priority)? {
                return Ok(false);
            }

            let vias = table.impacted_vias(&pattern, priority);
            self.snapshot.store(Arc::new(table.clone()));

            debug!("Adding route {pattern} => {action}, priority {priority}, source '{source}'");
            metrics::gauge!("bpa.rib.entries", "source" => source).increment(1.0);

            vias
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
        Ok(true)
    }

    pub(crate) async fn remove(
        &self,
        pattern: &EidPattern,
        source: &str,
        action: Action,
        priority: u32,
    ) -> bool {
        let pattern = self.expand_pattern(pattern.clone());
        let action = self.expand_action(action);

        {
            let mut table = self.table.lock();

            let entry = Entry {
                action: action.clone(),
                source: source.to_string(),
            };
            if !table.remove(&pattern, &entry, priority) {
                return false;
            }

            self.snapshot.store(Arc::new(table.clone()));
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
            let mut table = self.table.lock();
            let result = table.remove_by_source(source);
            self.snapshot.store(Arc::new(table.clone()));
            result
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

    pub async fn add_forward(&self, node_id: NodeId, peer: u32) -> Result<bool> {
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

    pub async fn add_service(&self, eid: Eid, service: Arc<Service>) -> Result<bool> {
        self.add(
            eid.into(),
            Self::SERVICES_NAME.into(),
            Action::Internal(InternalAction::Local(service)),
            self.service_priority,
        )
        .await
    }

    pub async fn remove_service(&self, eid: &Eid, service: Arc<Service>) -> bool {
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
        {
            let mut agents = self.agents.lock();
            let HashMapEntry::Vacant(e) = agents.entry(name.clone()) else {
                return Err(Error::AlreadyExists(name));
            };
            e.insert(agent.clone());
        }

        info!("Registered new routing agent: {name}");
        metrics::gauge!("bpa.rib.agents").increment(1.0);

        let node_ids: Vec<NodeId> = (&*self.node_ids).into();

        agent
            .on_register(Box::new(Sink::new(name, self.clone())), &node_ids)
            .await;

        Ok(node_ids)
    }

    pub(crate) async fn unregister_agent(&self, name: &str) {
        let agent = self.agents.lock().remove(name);

        if let Some(agent) = agent {
            metrics::gauge!("bpa.rib.agents").decrement(1.0);
            agent.on_unregister().await;
            self.remove_by_source(name).await;
            info!("Unregistered routing agent: {name}");
        }
    }

    pub(crate) async fn shutdown_agents(&self) {
        let agents = self.agents.lock().drain().collect::<Vec<_>>();

        if !agents.is_empty() {
            metrics::gauge!("bpa.rib.agents").decrement(agents.len() as f64);
        }

        for (name, agent) in agents {
            agent.on_unregister().await;
            self.remove_by_source(&name).await;
            info!("Unregistered routing agent: {name}");
        }
    }

    pub fn add_address_type(&self, address_type: ClaAddressType, cla: Arc<Cla>) {
        self.address_types.lock().insert(address_type, cla);
    }

    pub fn remove_address_type(&self, address_type: &ClaAddressType) {
        self.address_types.lock().remove(address_type);
    }

    pub(super) fn has_agent(&self, name: &str) -> bool {
        self.agents.lock().contains_key(name)
    }

    fn expand_pattern(&self, pattern: EidPattern) -> EidPattern {
        if let Some(ipn) = &self.node_ids.ipn {
            pattern.expand_local_node(ipn).unwrap_or(pattern)
        } else {
            pattern
        }
    }

    fn expand_action(&self, action: Action) -> Action {
        match action {
            Action::Route(RouteAction::Via(eid)) => Action::Route(RouteAction::Via(
                self.node_ids.expand_local_node(&eid).unwrap_or(eid),
            )),
            other => other,
        }
    }

    async fn notify_updated(&self) {
        // notify_one() stores a permit if the poll_waiting_task is mid-scan,
        // so a route change during an in-flight scan triggers a re-scan rather
        // than being lost (notify_waiters() stores nothing).
        self.poll_waiting_notify.notify_one();
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
    use core::num::NonZeroUsize;
    use core::time::Duration;

    use hardy_bpv7::bundle::Id as BundleId;
    use hardy_bpv7::creation_timestamp::CreationTimestamp;
    use hardy_bpv7::eid::{IpnNodeId, Service as EidService};

    use super::*;
    use crate::services::registry::ServiceImpl;
    use crate::services::tests::NullService;
    use crate::storage::{BundleMemStorage, MetadataMemStorage};

    fn make_rib() -> Arc<Rib> {
        let node_ids = Arc::new(NodeIds {
            ipn: Some(IpnNodeId {
                allocator_id: 0,
                node_number: 1,
            }),
            dtn: None,
        });

        let store = Arc::new(Store::new(
            NonZeroUsize::new(16).unwrap(),
            Arc::new(MetadataMemStorage::new(&Default::default())),
            Arc::new(BundleMemStorage::new(&Default::default())),
        ));

        Arc::new(Rib::new(node_ids, store, 1))
    }

    fn add_route(rib: &Rib, pattern: &str, source: &str, action: Action, priority: u32) {
        let pattern: EidPattern = pattern.parse().unwrap();
        let entry = Entry {
            action,
            source: source.to_string(),
        };
        let mut table = rib.table.lock();
        table.insert(pattern, entry, priority).unwrap();
        rib.snapshot.store(Arc::new(table.clone()));
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

    fn make_bundle(destination: &str) -> Bundle {
        Bundle {
            bundle: Bpv7Bundle {
                id: BundleId {
                    source: "ipn:0.99.1".parse().unwrap(),
                    timestamp: CreationTimestamp::now(),
                    fragment_info: None,
                },
                flags: Default::default(),
                crc_type: Default::default(),
                destination: destination.parse().unwrap(),
                report_to: Default::default(),
                lifetime: Duration::from_secs(3600),
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
        assert!(matches!(result, Some(DispatchAction::Forward(42))));
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
        assert!(matches!(result, Some(DispatchAction::Forward(99))));
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
        assert!(matches!(result, Some(DispatchAction::Forward(77))));
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
            Some(DispatchAction::Forward(p)) => p,
            other => panic!("Expected Forward, got {other:?}"),
        };

        let mut bundle2 = make_bundle("ipn:0.50.1");
        bundle2.bundle.id = bundle.bundle.id.clone();
        let result2 = rib.find(&mut bundle2);
        let peer2 = match result2 {
            Some(DispatchAction::Forward(p)) => p,
            other => panic!("Expected Forward, got {other:?}"),
        };

        assert_eq!(peer1, peer2, "ECMP selection must be deterministic");
        assert!(
            peer1 == 10 || peer1 == 11,
            "Peer must be one of the ECMP targets, got {peer1}"
        );
    }

    #[test]
    fn test_ecmp_direct_forwards() {
        let rib = make_rib();

        // Two direct forward peers for the same node (redundant CLA links)
        add_local_forward(&rib, ipn_node(2), 42);
        add_local_forward(&rib, ipn_node(2), 43);

        // find_peers returns both
        let peers = rib.find_peers(&"ipn:0.2.1".parse().unwrap());
        assert_eq!(peers, Some([42, 43].into()));

        // find resolves deterministically to one of them
        let mut bundle = make_bundle("ipn:0.2.1");
        let result = rib.find(&mut bundle);
        let peer = match result {
            Some(DispatchAction::Forward(p)) => p,
            other => panic!("Expected Forward, got {other:?}"),
        };
        assert!(
            peer == 42 || peer == 43,
            "Peer must be one of the direct forwards, got {peer}"
        );

        // Same bundle deterministically picks the same peer
        let mut bundle2 = make_bundle("ipn:0.2.1");
        bundle2.bundle.id = bundle.bundle.id.clone();
        let result2 = rib.find(&mut bundle2);
        let peer2 = match result2 {
            Some(DispatchAction::Forward(p)) => p,
            other => panic!("Expected Forward, got {other:?}"),
        };
        assert_eq!(peer, peer2, "ECMP selection must be deterministic");
    }

    #[test]
    fn test_admin_endpoint_lookup() {
        let rib = make_rib();
        let mut bundle = make_bundle("ipn:0.1.0");
        let result = rib.find(&mut bundle);
        assert!(
            matches!(result, Some(DispatchAction::AdminEndpoint)),
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
            Action::Internal(InternalAction::Local(Arc::new(Service {
                service: ServiceImpl::LowLevel(Arc::new(NullService)),
                service_id: EidService::Ipn(42),
            }))),
            1,
        );

        let mut bundle = make_bundle("ipn:0.1.42");
        let result = rib.find(&mut bundle);
        assert!(
            matches!(result, Some(DispatchAction::Deliver(_))),
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
            Action::Internal(InternalAction::Local(Arc::new(Service {
                service: ServiceImpl::LowLevel(Arc::new(NullService)),
                service_id: EidService::Ipn(42),
            }))),
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
            matches!(result, Some(DispatchAction::AdminEndpoint)),
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
                Some(DispatchAction::Drop(Some(
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
        assert!(
            matches!(result, Err(Error::NullNextHop)),
            "Via null endpoint should be rejected, got {result:?}"
        );
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
        assert!(
            matches!(result, Err(Error::ViaOwnNode(_))),
            "Via own node should be rejected, got {result:?}"
        );
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
        assert!(
            matches!(result, Ok(true)),
            "Default route should be accepted, got {result:?}"
        );
    }
}
