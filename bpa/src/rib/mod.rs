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
mod local;
mod route;

#[derive(Debug)]
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
    // Routing agent tracking: spin::Mutex for O(1) HashMap operations
    agents: hardy_async::sync::spin::Mutex<HashMap<String, Arc<agent::Agent>>>,
    node_ids: Arc<node_ids::NodeIds>,
    tasks: hardy_async::TaskPool,
    poll_waiting_notify: Arc<hardy_async::Notify>,
    store: Arc<storage::Store>,
}

impl Rib {
    pub(crate) fn new(node_ids: Arc<node_ids::NodeIds>, store: Arc<storage::Store>) -> Self {
        Self {
            inner: RwLock::new(RibInner {
                locals: local::LocalInner::new(&node_ids),
                routes: BTreeMap::new(),
                address_types: HashMap::new(),
            }),
            agents: Default::default(),
            node_ids,
            tasks: hardy_async::TaskPool::new(),
            poll_waiting_notify: Arc::new(hardy_async::Notify::new()),
            store,
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
pub(super) mod tests {
    use super::*;

    pub fn make_rib() -> Arc<Rib> {
        use hardy_bpv7::eid::IpnNodeId;

        let node_ids = Arc::new(node_ids::NodeIds {
            ipn: Some(IpnNodeId {
                allocator_id: 0,
                node_number: 1,
            }),
            dtn: None,
        });

        let store = Arc::new(storage::Store::new(
            core::num::NonZeroUsize::new(64).unwrap(),
            core::num::NonZeroUsize::new(4096).unwrap(),
            core::num::NonZeroUsize::new(16).unwrap(),
            Arc::new(storage::metadata_mem::MetadataMemStorage::new(
                &Default::default(),
            )),
            Arc::new(storage::bundle_mem::BundleMemStorage::new(
                &Default::default(),
            )),
        ));

        Arc::new(Rib::new(node_ids, store))
    }

    /// Add a route directly to the RIB's route table (sync, no store interaction).
    pub fn add_route(
        rib: &Rib,
        pattern: &str,
        source: &str,
        action: routes::Action,
        priority: u32,
    ) {
        let pattern: EidPattern = pattern.parse().unwrap();
        let entry = route::Entry {
            action,
            source: source.to_string(),
        };

        let mut inner = rib.inner.write();
        match inner.routes.entry(priority) {
            btree_map::Entry::Vacant(e) => {
                e.insert([(pattern, [entry].into())].into());
            }
            btree_map::Entry::Occupied(mut e) => match e.get_mut().entry(pattern) {
                btree_map::Entry::Vacant(pe) => {
                    pe.insert([entry].into());
                }
                btree_map::Entry::Occupied(mut pe) => {
                    pe.get_mut().insert(entry);
                }
            },
        }
    }

    /// Add a local forward entry directly (sync, no store interaction).
    pub fn add_local_forward(rib: &Rib, node_id: hardy_bpv7::eid::NodeId, peer: u32) {
        let pattern: EidPattern = node_id.into();
        let mut inner = rib.inner.write();
        match inner.locals.actions.entry(pattern) {
            btree_map::Entry::Vacant(e) => {
                e.insert([local::Action::Forward(peer)].into());
            }
            btree_map::Entry::Occupied(mut e) => {
                e.get_mut().insert(local::Action::Forward(peer));
            }
        }
    }

    #[test]
    fn test_impacted_subsets() {
        let rib = make_rib();

        // Add a Via route for ipn:2.0 at priority 10
        add_route(
            &rib,
            "ipn:*.*",
            "src",
            routes::Action::Via("ipn:0.2.0".parse().unwrap()),
            10,
        );

        // Add a more specific Drop route at priority 20 (lower priority)
        add_route(&rib, "ipn:0.3.*", "src", routes::Action::Drop(None), 20);

        // Verify both routes were inserted
        let inner = rib.inner.read();
        assert!(inner.routes.contains_key(&10));
        assert!(inner.routes.contains_key(&20));
    }
}
