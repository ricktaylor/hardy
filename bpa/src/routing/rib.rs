use core::hash::BuildHasher;

use foldhash::quality::RandomState;
use futures::{FutureExt, select_biased};
use hardy_async::sync::RwLock;
use hardy_async::{Notify, TaskPool};
use hardy_bpv7::bundle::Bundle as Bpv7Bundle;
use hardy_bpv7::eid::{Eid, NodeId};
use hardy_bpv7::status_report::ReasonCode;
use hardy_eid_patterns::EidPattern;
use tracing::{debug, info, trace};

#[cfg(feature = "instrument")]
use tracing::instrument;

use super::table::{
    AtomicRouteTable, Error as TableError, FindResult as TableFindResult, InternalAction,
};
use super::{Agent, RoutingAgent};
use crate::bundle::{self, BundleMetadata};
use crate::cla::{ClaAddressType, registry::Cla as ClaEntry};
use crate::dispatcher::Dispatcher;
use crate::node_ids::NodeIds;
use crate::services::registry::Service;
use crate::storage::Store;
use crate::{Arc, HashMap, HashSet};

#[derive(Debug)]
pub enum FindResult {
    AdminEndpoint,
    Deliver(Arc<Service>),    // Deliver to local service
    Forward(u32),             // Forward to peer
    Drop(Option<ReasonCode>), // Drop with reason code
}

struct RibInner {
    table: AtomicRouteTable,
    address_types: HashMap<ClaAddressType, Arc<ClaEntry>>,
}

pub struct Rib {
    inner: RwLock<RibInner>,
    agents: hardy_async::sync::spin::Mutex<HashMap<String, Arc<Agent>>>,
    node_ids: Arc<NodeIds>,
    ecmp_hash_state: RandomState,
    pub(super) tasks: TaskPool,
    poll_waiting_notify: Arc<Notify>,
    store: Arc<Store>,
    service_priority: u32,
}

pub(crate) struct RibBuilder {
    agents: Vec<(String, Arc<dyn RoutingAgent>)>,

    // The priority for services - default 1
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

    pub async fn build(self, node_ids: Arc<NodeIds>, store: Arc<Store>) -> super::Result<Arc<Rib>> {
        let rib = Arc::new(Rib::new(node_ids, store, self.service_priority));
        for (name, agent) in self.agents {
            rib.register_agent(name, agent).await?;
        }
        Ok(rib)
    }
}

impl Rib {
    pub(super) const FORWARDS_NAME: &str = "neighbours";
    pub(super) const SERVICES_NAME: &str = "services";

    pub(super) fn new(node_ids: Arc<NodeIds>, store: Arc<Store>, service_priority: u32) -> Self {
        let table = AtomicRouteTable::new(&node_ids);

        Self {
            inner: RwLock::new(RibInner {
                table,
                address_types: HashMap::new(),
            }),
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

        // Signal initial poll to pick up any pre-existing Waiting bundles
        self.poll_waiting_notify.notify_one();
    }

    pub async fn shutdown(&self) {
        self.tasks.shutdown().await;
    }

    pub(super) async fn notify_updated(&self) {
        self.poll_waiting_notify.notify_waiters();
    }

    pub fn add_address_type(&self, address_type: ClaAddressType, cla: Arc<ClaEntry>) {
        self.inner.write().address_types.insert(address_type, cla);
    }

    pub fn remove_address_type(&self, address_type: &ClaAddressType) {
        self.inner.write().address_types.remove(address_type);
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    pub fn find(&self, bundle: &mut bundle::Bundle) -> Option<FindResult> {
        let inner = self.inner.read();

        let result =
            inner
                .table
                .find_recurse(&bundle.bundle.destination, true, &mut HashSet::new())?;
        if !matches!(result, TableFindResult::Reflect) {
            return map_result(
                result,
                &self.ecmp_hash_state,
                &bundle.bundle,
                &mut bundle.metadata,
            );
        }

        // Reflect: return the bundle via the previous forwarding node,
        // falling back to the bundle source as last resort.
        let previous = bundle
            .previous_node()
            .unwrap_or_else(|| bundle.bundle.id.source.clone());

        map_result(
            inner
                .table
                .find_recurse(&previous, false, &mut HashSet::new())?,
            &self.ecmp_hash_state,
            &bundle.bundle,
            &mut bundle.metadata,
        )
    }

    /// Find a registered local service matching the given EID.
    ///
    /// Used for status report notifications (`admin.rs`) where we need to
    /// find the service to notify, regardless of routing policy. This
    /// intentionally bypasses priority ordering and Drop rules: a Drop
    /// rule prevents routing bundles to a service, but should not prevent
    /// the BPA from notifying a registered service about its own bundles.
    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(to = %to)))]
    pub fn find_service(&self, to: &Eid) -> Option<Arc<Service>> {
        let inner = self.inner.read();
        inner.table.find_service(to)
    }
}

fn map_result(
    result: TableFindResult,
    ecmp_hash_state: &RandomState,
    bundle: &Bpv7Bundle,
    metadata: &mut BundleMetadata,
) -> Option<FindResult> {
    match result {
        TableFindResult::AdminEndpoint => Some(FindResult::AdminEndpoint),
        TableFindResult::Deliver(service) => Some(FindResult::Deliver(service)),
        TableFindResult::Drop(reason) => Some(FindResult::Drop(reason)),
        TableFindResult::Forward(peers) if peers.is_empty() => {
            debug_assert!(false, "Empty Forward result from find_recurse");
            None
        }
        TableFindResult::Forward(mut peers) => {
            if tracing::enabled!(tracing::Level::TRACE) {
                trace!(
                    "Forward to CLA peer{} {}",
                    if peers.len() == 1 { "" } else { "s:" },
                    peers.iter().fold(String::new(), |acc, (k, v)| {
                        if acc.is_empty() {
                            format!("{k} ({v})")
                        } else {
                            format!("{acc}, {k} ({v})")
                        }
                    })
                );
            }

            let idx = if peers.len() > 1 {
                (ecmp_hash_state.hash_one((
                    &bundle.id.source,
                    &bundle.destination,
                    &metadata.writable.flow_label,
                )) % (peers.len() as u64)) as usize
            } else {
                0
            };
            let (peer, next_hop) = peers.swap_remove(idx);

            // Set the next-hop for Egress filters
            metadata.read_only.next_hop = Some(next_hop.clone());

            Some(FindResult::Forward(peer))
        }
        TableFindResult::Reflect => None,
    }
}

impl Rib {
    pub(crate) async fn register_agent(
        self: &Arc<Self>,
        name: String,
        agent: Arc<dyn RoutingAgent>,
    ) -> super::Result<Vec<NodeId>> {
        let agent = {
            let mut agents = self.agents.lock();
            let crate::hash_map::Entry::Vacant(e) = agents.entry(name.clone()) else {
                return Err(super::Error::AlreadyExists(name));
            };

            info!("Registered new routing agent: {name}");

            e.insert(Arc::new(Agent { agent, name })).clone()
        };

        metrics::gauge!("bpa.rib.agents").increment(1.0);

        let node_ids: Vec<NodeId> = (&*self.node_ids).into();

        agent
            .agent
            .on_register(
                Box::new(super::sink::Sink::new(Arc::downgrade(&agent), self.clone())),
                &node_ids,
            )
            .await;

        Ok(node_ids)
    }

    pub(crate) async fn unregister_agent(&self, agent: Arc<Agent>) {
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

    pub(crate) async fn update_routes(
        &self,
        source: &str,
        add: &[super::route::Route],
        remove: &[super::route::Route],
    ) -> Result<(), TableError> {
        {
            let mut inner = self.inner.write();
            let mut vt = inner.table.virtual_table(source, &self.node_ids);
            for r in remove {
                vt.remove(&r.pattern, &r.action, r.priority);
            }
            for r in add {
                vt.insert(r.pattern.clone(), r.action.clone(), r.priority)?;
            }
            inner.table = vt.commit()?;
        }

        self.notify_updated().await;
        Ok(())
    }

    pub async fn remove_by_source(&self, source: &str) {
        let removed_count = {
            let mut inner = self.inner.write();
            inner.table.remove_by_source(source)
        };

        if removed_count == 0 {
            return;
        }

        debug!("Removed all routes from source '{source}'");
        metrics::gauge!("bpa.rib.entries", "source" => source.to_string())
            .decrement(removed_count as f64);

        self.notify_updated().await;
    }

    pub async fn add_forward(&self, node_id: NodeId, peer: u32) -> bool {
        let pattern: EidPattern = node_id.into();
        {
            let mut inner = self.inner.write();
            inner.table.insert(
                pattern,
                InternalAction::Forward(peer),
                0,
                Self::FORWARDS_NAME,
            );
        }
        self.notify_updated().await;
        true
    }

    pub async fn remove_forward(&self, node_id: NodeId, peer: u32) -> bool {
        let pattern: EidPattern = node_id.into();
        let removed = {
            let mut inner = self.inner.write();
            inner.table.remove(
                &pattern,
                &InternalAction::Forward(peer),
                0,
                Self::FORWARDS_NAME,
            )
        };
        if removed && self.store.reset_peer_queue(peer).await {
            self.notify_updated().await;
        }
        removed
    }

    pub async fn add_service(&self, eid: Eid, service: Arc<Service>) -> bool {
        {
            let mut inner = self.inner.write();
            inner.table.insert(
                eid.into(),
                InternalAction::Local(service),
                self.service_priority,
                Self::SERVICES_NAME,
            );
        }
        self.notify_updated().await;
        true
    }

    pub async fn remove_service(&self, eid: &Eid, service: Arc<Service>) -> bool {
        let pattern: EidPattern = eid.clone().into();
        let removed = {
            let mut inner = self.inner.write();
            inner.table.remove(
                &pattern,
                &InternalAction::Local(service),
                self.service_priority,
                Self::SERVICES_NAME,
            )
        };
        if removed {
            self.notify_updated().await;
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::table::action::{Action, InternalAction, RouteAction};
    use crate::services::registry::ServiceImpl;
    use crate::{BTreeSet, bundle, storage};

    pub(super) fn make_rib() -> Arc<Rib> {
        use hardy_bpv7::eid::IpnNodeId;

        let node_ids = Arc::new(NodeIds {
            ipn: Some(IpnNodeId {
                allocator_id: 0,
                node_number: 1,
            }),
            dtn: None,
        });

        let store = Arc::new(Store::new(
            core::num::NonZeroUsize::new(16).unwrap(),
            Arc::new(storage::MetadataMemStorage::new(&Default::default())),
            Arc::new(storage::BundleMemStorage::new(&Default::default())),
        ));

        Arc::new(Rib::new(node_ids, store, 1))
    }

    pub(super) fn add_route(rib: &Rib, pattern: &str, source: &str, action: Action, priority: u32) {
        let pattern: EidPattern = pattern.parse().unwrap();
        let mut inner = rib.inner.write();
        match action {
            Action::Internal(internal) => {
                inner.table.insert(pattern, internal, priority, source);
            }
            Action::Route(route_action) => {
                let mut vt = inner.table.virtual_table(source, &rib.node_ids);
                let _ = vt.insert(pattern, route_action, priority);
                if let Ok(new_table) = vt.commit() {
                    inner.table = new_table;
                }
            }
        }
    }

    fn make_bundle(destination: &str) -> bundle::Bundle {
        bundle::Bundle {
            bundle: Bpv7Bundle {
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

    #[test]
    fn test_exact_match() {
        let rib = make_rib();

        // Add a local forward peer for ipn:0.2.*
        add_route(
            &rib,
            "ipn:0.2.*",
            "forward",
            Action::Internal(InternalAction::Forward(42)),
            0,
        );

        // Lookup for an EID under that node
        let mut bundle = make_bundle("ipn:0.2.1");
        let result = rib.find(&mut bundle);
        assert!(matches!(result, Some(FindResult::Forward(42))));
    }

    #[test]
    fn test_default_route() {
        let rib = make_rib();

        // Add a Via route for a specific remote range (not catch-all, to avoid
        // self-referential validation since *:** matches its own Via target)
        add_route(
            &rib,
            "ipn:0.50.*",
            "default",
            Action::Route(RouteAction::Via("ipn:0.10.0".parse().unwrap())),
            1000,
        );

        // Add a local forward for the gateway node
        add_route(
            &rib,
            "ipn:0.10.*",
            "forward",
            Action::Internal(InternalAction::Forward(99)),
            0,
        );

        // The destination should resolve via the route
        let mut bundle = make_bundle("ipn:0.50.1");
        let result = rib.find(&mut bundle);
        assert!(matches!(result, Some(FindResult::Forward(99))));
    }

    #[test]
    fn test_no_route() {
        let rib = make_rib();

        // No matching route: unknown destination returns None (wait for route)
        let mut bundle = make_bundle("ipn:0.50.1");
        let result = rib.find(&mut bundle);
        assert!(result.is_none());
    }

    #[test]
    fn test_recursion_loop() {
        let rib = make_rib();

        // Create a routing loop: ipn:0.2.* -> Via ipn:0.3.0, ipn:0.3.* -> Via ipn:0.2.0
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

        // Add a Reflect route for ipn:0.5.*
        add_route(
            &rib,
            "ipn:0.5.*",
            "reflect",
            Action::Route(RouteAction::Reflect),
            10,
        );

        // Add a forward peer for node 4 (the previous hop)
        add_route(
            &rib,
            "ipn:0.4.*",
            "forward",
            Action::Internal(InternalAction::Forward(77)),
            0,
        );

        // Bundle with a previous node set
        let mut bundle = make_bundle("ipn:0.5.1");
        bundle.bundle.previous_node = Some("ipn:0.4.0".parse().unwrap());

        let result = rib.find(&mut bundle);
        // Should route back to the previous node's peer
        assert!(matches!(result, Some(FindResult::Forward(77))));
    }

    #[test]
    fn test_reflection_no_double() {
        let rib = make_rib();

        // Reflect routes for both destination and previous-hop: should not
        // double-reflect (return None instead)
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

        // Two Via routes at the same priority, each resolving to a different peer
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

        // Add forward peers for both gateways
        add_route(
            &rib,
            "ipn:0.10.*",
            "forward",
            Action::Internal(InternalAction::Forward(10)),
            0,
        );
        add_route(
            &rib,
            "ipn:0.11.*",
            "forward",
            Action::Internal(InternalAction::Forward(11)),
            0,
        );

        // Same bundle should always hash to the same peer (deterministic)
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

        // Rib::new() adds admin endpoint routes at priority 0.
        // The IPN admin EID (ipn:0.1.0) should resolve to AdminEndpoint.
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

        // A bundle for a local service number with no registered service
        // should return None (wait for route), not Drop.
        // This is the correct DTN behaviour: default to wait.
        let mut bundle = make_bundle("ipn:0.1.99");
        let result = rib.find(&mut bundle);
        assert!(
            result.is_none(),
            "Unregistered local service should wait (no route), got {result:?}"
        );
    }

    #[test]
    fn test_concrete_service_matches() {
        // A service route stored with a concrete local EID pattern
        // should match bundles destined for that EID.
        let rib = make_rib();

        add_route(
            &rib,
            "ipn:0.1.42",
            "services",
            Action::Internal(InternalAction::Local(Arc::new(Service {
                service: ServiceImpl::LowLevel(Arc::new(crate::services::tests::NullService)),
                service_id: hardy_bpv7::eid::Service::Ipn(42),
            }))),
            1,
        );

        let mut bundle = make_bundle("ipn:0.1.42");
        let result = rib.find(&mut bundle);
        assert!(
            matches!(result, Some(FindResult::Deliver(_))),
            "Concrete local EID should match concrete service route, got {result:?}"
        );
    }

    #[test]
    fn test_concrete_service_ignores_remote_eid() {
        // A concrete service route should NOT match a remote node's EID,
        // even if the service number matches.
        let rib = make_rib();

        add_route(
            &rib,
            "ipn:0.1.42",
            "services",
            Action::Internal(InternalAction::Local(Arc::new(Service {
                service: ServiceImpl::LowLevel(Arc::new(crate::services::tests::NullService)),
                service_id: hardy_bpv7::eid::Service::Ipn(42),
            }))),
            1,
        );

        // Bundle for a different node should NOT match
        let mut bundle = make_bundle("ipn:0.2.42");
        let result = rib.find(&mut bundle);
        assert!(
            result.is_none(),
            "Remote EID should not match local service route, got {result:?}"
        );
    }

    #[test]
    fn test_admin_endpoint_matches_concrete() {
        // The admin endpoint is registered with a concrete IPN EID (ipn:0.1.0).
        // A bundle for that EID should match directly.
        let rib = make_rib();

        let mut bundle = make_bundle("ipn:0.1.0");
        let result = rib.find(&mut bundle);
        assert!(
            matches!(result, Some(FindResult::AdminEndpoint)),
            "Concrete admin EID should match admin endpoint route, got {result:?}"
        );
    }

    #[test]
    fn test_explicit_drop_overrides_wait() {
        let rib = make_rib();

        // Operator configures an explicit Drop rule for a service range.
        // This overrides the default wait behaviour for unregistered services.
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

    #[test]
    fn test_action_precedence() {
        let drop_action = RouteAction::Drop(None);
        let reflect_action = RouteAction::Reflect;
        let via_action = RouteAction::Via("ipn:1.0".parse().unwrap());

        assert!(drop_action < reflect_action);
        assert!(reflect_action < via_action);
        assert!(drop_action < via_action);
    }

    #[test]
    fn test_local_action_sort() {
        let admin = InternalAction::AdminEndpoint;
        let forward_1 = InternalAction::Forward(1);
        let forward_2 = InternalAction::Forward(2);

        assert!(admin < forward_1);
        assert!(forward_1 < forward_2);

        let mut set = BTreeSet::new();
        set.insert(forward_2.clone());
        set.insert(admin.clone());
        set.insert(forward_1.clone());

        let sorted: Vec<_> = set.into_iter().collect();
        assert_eq!(sorted[0], admin);
        assert_eq!(sorted[1], forward_1);
        assert_eq!(sorted[2], forward_2);
    }
}
