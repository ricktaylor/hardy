use super::*;
use hardy_bpv7::{
    eid::{Eid, NodeId},
    status_report::ReasonCode,
};
use hardy_eid_patterns::EidPattern;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::RwLock,
};

mod find;
mod local;
mod route;

pub enum FindResult {
    AdminEndpoint,
    Deliver(Option<Arc<service_registry::Service>>), // Deliver to local service
    Forward(u32),                                    // Forward to peer
    Drop(Option<ReasonCode>),                        // Drop with reason code
}

type RouteTable = BTreeMap<u32, BTreeMap<EidPattern, BTreeSet<route::Entry>>>; // priority -> pattern -> set of entries

struct RibInner {
    locals: local::LocalInner,
    routes: RouteTable,
    address_types: HashMap<cla::ClaAddressType, Arc<cla::registry::Cla>>,
}

pub struct Rib {
    inner: RwLock<RibInner>,
    tasks: hardy_async::task_pool::TaskPool,
    poll_waiting_notify: Arc<tokio::sync::Notify>,
    store: Arc<storage::Store>,
}

impl Rib {
    pub fn new(config: &config::Config, store: Arc<storage::Store>) -> Self {
        Self {
            inner: RwLock::new(RibInner {
                locals: local::LocalInner::new(config),
                routes: BTreeMap::new(),
                address_types: HashMap::new(),
            }),
            tasks: hardy_async::task_pool::TaskPool::new(),
            poll_waiting_notify: Arc::new(tokio::sync::Notify::new()),
            store,
        }
    }

    pub fn start(self: &Arc<Self>, dispatcher: Arc<dispatcher::Dispatcher>) {
        let cancel_token = self.tasks.cancel_token().clone();
        let rib = self.clone();
        hardy_async::spawn!(self.tasks, "poll_waiting_task", async move {
            loop {
                tokio::select! {
                    _ = rib.poll_waiting_notify.notified() => {
                        dispatcher.poll_waiting(cancel_token.clone()).await;
                    },
                    _ = cancel_token.cancelled() => {
                        break;
                    }
                }
            }

            debug!("Poll waiting task complete");
        });
    }

    pub async fn shutdown(&self) {
        self.tasks.shutdown().await;
    }

    async fn notify_updated(&self) {
        self.poll_waiting_notify.notify_one();
    }

    pub fn add_address_type(
        &self,
        address_type: cla::ClaAddressType,
        cla: Arc<cla::registry::Cla>,
    ) {
        self.inner
            .write()
            .trace_expect("Failed to lock mutex")
            .address_types
            .insert(address_type, cla);
    }

    pub fn remove_address_type(&self, address_type: &cla::ClaAddressType) {
        self.inner
            .write()
            .trace_expect("Failed to lock mutex")
            .address_types
            .remove(address_type);
    }
}

#[cfg(test)]
mod tests {
    // use super::*;

    // // TODO: Implement test for 'Impacted Subsets' (Verify Rib::add detects affected sub-routes)
    // #[test]
    // fn test_impacted_subsets() {
    //     todo!("Verify Rib::add detects affected sub-routes");
    // }
}
