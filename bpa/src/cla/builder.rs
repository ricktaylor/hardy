use alloc::sync::Arc;

use tracing::info;

use crate::cla::{self, Cla as ClaTrait};
use crate::dispatcher::Dispatcher;
use crate::node_ids::NodeIds;
use crate::policy::{self, EgressPolicy};
use crate::rib::Rib;
use crate::storage::Store;
use crate::{HashMap, hash_map};

use super::peers::PeerTable;
use super::registry::{ClaEntry, ClaRegistry};

pub(crate) struct ClaRegistryBuilder {
    clas: HashMap<String, Arc<ClaEntry>>,
}

impl ClaRegistryBuilder {
    pub fn new() -> Self {
        Self {
            clas: Default::default(),
        }
    }

    pub fn insert(
        &mut self,
        name: String,
        cla: Arc<dyn ClaTrait>,
        policy: Option<Arc<dyn EgressPolicy>>,
    ) -> cla::Result<()> {
        let hash_map::Entry::Vacant(e) = self.clas.entry(name.clone()) else {
            return Err(cla::Error::AlreadyExists(name));
        };
        info!("Inserted CLA: {name}");
        e.insert(Arc::new(ClaEntry {
            cla,
            peers: Default::default(),
            name: Arc::from(name.as_str()),
            policy: policy.unwrap_or_else(|| Arc::new(policy::null_policy::EgressPolicy::new())),
        }));
        Ok(())
    }

    pub async fn build(
        self,
        node_ids: &Arc<NodeIds>,
        poll_channel_depth: usize,
        rib: &Arc<Rib>,
        store: &Arc<Store>,
        dispatcher: &Arc<Dispatcher>,
    ) -> cla::Result<Arc<ClaRegistry>> {
        let peers = Arc::new(PeerTable::new());
        let (drop_tx, drop_rx) = flume::unbounded();
        let tasks = hardy_async::TaskPool::new();

        let registry = Arc::new(ClaRegistry::new(
            node_ids.clone(),
            rib.clone(),
            store.clone(),
            peers,
            poll_channel_depth,
            tasks.clone(),
            drop_tx,
        ));

        let reconciler_registry = registry.clone();
        hardy_async::spawn!(tasks, "cla_drop_reconciler", async move {
            while let Ok(weak_cla) = drop_rx.recv_async().await {
                if let Some(cla) = weak_cla.upgrade() {
                    reconciler_registry.unregister(&cla.name).await;
                }
            }
        });

        for (_, cla) in self.clas {
            registry
                .register(
                    cla.name.to_string(),
                    cla.cla.clone(),
                    dispatcher,
                    Some(cla.policy.clone()),
                )
                .await?;
        }

        Ok(registry)
    }
}
