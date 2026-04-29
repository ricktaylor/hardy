use super::*;
use futures::{FutureExt, select_biased};
use hardy_async::sync::RwLock;
use hardy_bpv7::{
    eid::{Eid, NodeId},
    status_report::ReasonCode,
};
use hardy_eid_patterns::EidPattern;

pub(crate) mod agent;

mod find;
mod route;

#[derive(Debug)]
pub enum FindResult {
    AdminEndpoint,
    Deliver(Arc<services::registry::Service>), // Deliver to local service
    Forward(u32),                              // Forward to peer
    Drop(Option<ReasonCode>),                  // Drop with reason code
}

type RouteTable = BTreeMap<u32, BTreeMap<EidPattern, BTreeSet<route::Entry>>>; // priority -> pattern -> set of entries

struct RibInner {
    routes: RouteTable,
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
    tasks: hardy_async::TaskPool,
    poll_waiting_notify: Arc<hardy_async::Notify>,
    store: Arc<storage::Store>,

    // The priority for services - default 1
    service_priority: u32,
}

pub(crate) struct RibBuilder {
    agents: Vec<(String, Arc<dyn routes::RoutingAgent>)>,

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

    pub fn insert(&mut self, name: String, agent: Arc<dyn routes::RoutingAgent>) {
        self.agents.push((name, agent));
    }

    pub fn service_priority(&mut self, priority: u32) {
        self.service_priority = priority;
    }

    pub async fn build(
        self,
        node_ids: Arc<node_ids::NodeIds>,
        store: Arc<storage::Store>,
    ) -> routes::Result<Arc<Rib>> {
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
        store: Arc<storage::Store>,
        service_priority: u32,
    ) -> Self {
        let entry = route::Entry {
            source: Self::SERVICES_NAME.into(),
            action: route::Action::AdminEndpoint,
        };

        // Add localnode admin endpoint
        let mut admin_endpoints = BTreeMap::new();
        admin_endpoints.insert(NodeId::LocalNode.into(), [entry.clone()].into());

        if let Some(node_id) = &node_ids.ipn {
            // Add the Admin Endpoint EID itself (exact match, not wildcard)
            // Convert to Eid first to get ipn:N.0, then to EidPattern for exact match
            let admin_eid: Eid = (*node_id).into();
            admin_endpoints.insert(admin_eid.into(), [entry.clone()].into());
        }

        if let Some(node_name) = &node_ids.dtn {
            // Add the Admin Endpoint EID itself (exact match, not wildcard)
            let admin_eid: Eid = node_name.clone().into();
            admin_endpoints.insert(admin_eid.into(), [entry].into());
        }

        let mut routes = BTreeMap::new();
        routes.insert(0, admin_endpoints);

        Self {
            inner: RwLock::new(RibInner {
                routes,
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

        // Signal initial poll to pick up any pre-existing Waiting bundles
        self.poll_waiting_notify.notify_one();
    }

    pub async fn shutdown(&self) {
        self.tasks.shutdown().await;
    }

    async fn notify_updated(&self) {
        self.poll_waiting_notify.notify_waiters();
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
}
