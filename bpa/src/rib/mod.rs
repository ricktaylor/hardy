use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use futures::{FutureExt, select_biased};
use hardy_async::TaskPool;
use hardy_async::sync::RwLock;
use hardy_bpv7::eid::{Eid, NodeId};
use hardy_bpv7::status_report::ReasonCode;
use hardy_eid_patterns::EidPattern;
use tracing::debug;

use crate::cla;
use crate::dispatcher::Dispatcher;
use crate::node_ids::NodeIds;
use crate::routes::{self, RoutingAgent};
use crate::services;
use crate::storage::Store;
use crate::{HashMap, HashSet, btree_map, bundle};

#[cfg(test)]
use crate::{node_ids, storage};

#[cfg(feature = "instrument")]
use tracing::instrument;
use tracing::{trace, warn};

pub(crate) mod agent;
pub mod context;

mod find;
mod route;

#[derive(Debug)]
pub enum FindResult {
    AdminEndpoint,
    Deliver(Arc<services::registry::Service>),
    Forward(u32),
    Drop(Option<ReasonCode>),
}

type RouteTable = BTreeMap<u32, BTreeMap<EidPattern, BTreeSet<route::Entry>>>;

struct RibInner {
    routes: RouteTable,
    address_types: HashMap<cla::ClaAddressType, Arc<cla::registry::Cla>>,
}

pub struct Rib {
    inner: RwLock<RibInner>,
    agents: hardy_async::sync::spin::Mutex<HashMap<String, Arc<agent::Agent>>>,
    node_ids: Arc<NodeIds>,
    ecmp_hash_state: foldhash::quality::RandomState,
    tasks: TaskPool,
    poll_waiting_notify: Arc<hardy_async::Notify>,
    store: Arc<Store>,
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
        node_ids: Arc<NodeIds>,
        store: Arc<Store>,
    ) -> routes::Result<Arc<Rib>> {
        let rib = Arc::new(Rib::new(node_ids, store, self.service_priority));
        for (name, agent) in self.agents {
            rib.register_agent(name, agent).await?;
        }
        Ok(rib)
    }
}

impl Rib {
    const ADMIN_NAME: &str = "administrative endpoint";
    const FORWARDS_NAME: &str = "neighbours";
    const SERVICES_NAME: &str = "services";

    fn new(node_ids: Arc<NodeIds>, store: Arc<Store>, service_priority: u32) -> Self {
        let entry = route::Entry {
            source: Self::ADMIN_NAME.into(),
            action: route::Action::AdminEndpoint,
        };

        let mut admin_endpoints = BTreeMap::new();
        if let Some(node_name) = &node_ids.dtn {
            let admin_eid: Eid = node_name.clone().into();
            admin_endpoints.insert(admin_eid.into(), [entry.clone()].into());
        }

        if let Some(node_number) = &node_ids.ipn {
            let admin_eid: Eid = (*node_number).into();
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
            tasks: TaskPool::new(),
            poll_waiting_notify: Arc::new(hardy_async::Notify::new()),
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
