use super::*;
use futures::{FutureExt, select_biased};
use hardy_async::sync::RwLock;
use hardy_bpv7::{
    eid::{Eid, NodeId},
    status_report::ReasonCode,
};
use hardy_eid_patterns::EidPattern;

mod find;
mod local;
mod route;

pub enum FindResult {
    AdminEndpoint,
    Deliver(Option<Arc<services::registry::Service>>), // Deliver to local service
    Forward(u32),                                      // Forward to peer
    Drop(Option<ReasonCode>),                          // Drop with reason code
}

type RouteTable = BTreeMap<u32, BTreeMap<EidPattern, BTreeSet<route::Entry>>>; // priority -> pattern -> set of entries

struct RibInner {
    locals: local::LocalInner,
    routes: RouteTable,
    address_types: HashMap<cla::ClaAddressType, Arc<cla::registry::Cla>>,
}

pub struct Rib {
    inner: RwLock<RibInner>,
    tasks: hardy_async::TaskPool,
    poll_waiting_notify: Arc<hardy_async::Notify>,
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
            tasks: hardy_async::TaskPool::new(),
            poll_waiting_notify: Arc::new(hardy_async::Notify::new()),
            store,
        }
    }

    pub fn start(self: &Arc<Self>, dispatcher: Arc<dispatcher::Dispatcher>) {
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

#[cfg(test)]
mod tests {
    // use super::*;

    // // TODO: Implement test for 'Impacted Subsets' (Verify Rib::add detects affected sub-routes)
    // #[test]
    // fn test_impacted_subsets() {
    //     todo!("Verify Rib::add detects affected sub-routes");
    // }
}
