use futures::{FutureExt, select_biased};
use hardy_async::sync::RwLock;
use hardy_bpv7::{eid::Eid, status_report::ReasonCode};
use hardy_eid_patterns::EidPattern;
use tracing::{debug, info};

use super::agent;
use super::table::RouteTable;
use super::{Error, Result, RoutingAgent};
use crate::{Arc, HashMap, hash_map};
use crate::{cla, dispatcher, node_ids, services, storage};

#[derive(Debug)]
pub enum FindResult {
    AdminEndpoint,
    Deliver(Arc<services::registry::Service>), // Deliver to local service
    Forward(u32),                              // Forward to peer
    Drop(Option<ReasonCode>),                  // Drop with reason code
}

pub(super) struct RibInner {
    pub(super) table: RouteTable,
    pub(super) address_types: HashMap<cla::ClaAddressType, Arc<cla::registry::Cla>>,
}

pub struct Rib {
    pub(super) inner: RwLock<RibInner>,
    // Routing agent tracking: spin::Mutex for O(1) HashMap operations
    pub(super) agents: hardy_async::sync::spin::Mutex<HashMap<String, Arc<agent::Agent>>>,
    pub(super) node_ids: Arc<node_ids::NodeIds>,
    // Fixed per-instance seed for deterministic ECMP peer selection.
    // Random across BPA instances (unpredictable), but consistent within
    // an instance so the same bundle always selects the same peer.
    pub(super) ecmp_hash_state: foldhash::quality::RandomState,
    pub(crate) tasks: hardy_async::TaskPool,
    poll_waiting_notify: Arc<hardy_async::Notify>,
    pub(super) store: Arc<storage::Store>,

    // The priority for services - default 1
    pub(super) service_priority: u32,
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
    pub(super) const FORWARDS_NAME: &str = "neighbours";
    pub(super) const SERVICES_NAME: &str = "services";

    pub(super) fn new(
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

        // Signal initial poll to pick up any pre-existing Waiting bundles
        self.poll_waiting_notify.notify_one();
    }

    pub async fn shutdown(&self) {
        self.tasks.shutdown().await;
    }

    pub(super) async fn notify_updated(&self) {
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
}
