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
    cancel_token: tokio_util::sync::CancellationToken,
    task_tracker: tokio_util::task::TaskTracker,
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
            cancel_token: tokio_util::sync::CancellationToken::new(),
            task_tracker: tokio_util::task::TaskTracker::new(),
            poll_waiting_notify: Arc::new(tokio::sync::Notify::new()),
            store,
        }
    }

    pub fn start(self: &Arc<Self>, dispatcher: Arc<dispatcher::Dispatcher>) {
        let cancel_token = self.cancel_token.clone();
        let rib = self.clone();
        let task = async move {
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
        };

        #[cfg(feature = "tracing")]
        let task = {
            let span = tracing::trace_span!(parent: None, "poll_waiting_task");
            span.follows_from(tracing::Span::current());
            task.instrument(span)
        };

        self.task_tracker.spawn(task);
    }

    pub async fn shutdown(&self) {
        self.cancel_token.cancel();
        self.task_tracker.close();
        self.task_tracker.wait().await;
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
