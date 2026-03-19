mod find;
mod local;
mod route;

pub use local::*;

use futures::{FutureExt, select_biased};
use hardy_async::sync::RwLock;
use hardy_async::{Notify, TaskPool};
use hardy_bpv7::status_report::ReasonCode;
use hardy_eid_patterns::EidPattern;
use tracing::debug;

use crate::cla::{ClaAddressType, ClaRecord};
use crate::dispatcher::Dispatcher;
use crate::rib::local::LocalInner;
use crate::rib::route::Entry;
use crate::services::ServiceRecord;
use crate::storage::Store;
use crate::{Arc, BTreeMap, BTreeSet, HashMap, NodeIds};

pub enum FindResult {
    AdminEndpoint,
    Deliver(Option<Arc<ServiceRecord>>),
    Forward(u32),
    Drop(Option<ReasonCode>),
}

type RouteTable = BTreeMap<u32, BTreeMap<EidPattern, BTreeSet<Entry>>>; // priority -> pattern -> set of entries

struct RibInner {
    locals: LocalInner,
    routes: RouteTable,
    address_types: HashMap<ClaAddressType, Arc<ClaRecord>>,
}

pub struct Rib {
    inner: RwLock<RibInner>,
    tasks: TaskPool,
    poll_waiting_notify: Arc<Notify>,
    store: Arc<Store>,
}

impl Rib {
    pub fn new(node_ids: NodeIds, store: Arc<Store>) -> Self {
        Self {
            inner: RwLock::new(RibInner {
                locals: local::LocalInner::new(&node_ids),
                routes: BTreeMap::new(),
                address_types: HashMap::new(),
            }),
            tasks: TaskPool::new(),
            poll_waiting_notify: Arc::new(Notify::new()),
            store,
        }
    }

    pub fn start(self: &Arc<Self>, dispatcher: Arc<Dispatcher>) {
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
        self.poll_waiting_notify.notify_one();
    }

    pub fn add_address_type(&self, address_type: ClaAddressType, cla: Arc<ClaRecord>) {
        self.inner.write().address_types.insert(address_type, cla);
    }

    pub fn remove_address_type(&self, address_type: &ClaAddressType) {
        self.inner.write().address_types.remove(address_type);
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
