use trace_err::*;

use super::ClaAddress;
use super::egress_queue;
use super::entry::ClaEntry;
use crate::dispatcher::Dispatcher;
use crate::{Arc, HashMap, Weak};
use crate::{bundle, policy, storage};

/// A fully initialized peer with its egress queues.
pub struct Peer {
    cla: Weak<ClaEntry>,
    queues: HashMap<Option<u32>, storage::channel::Sender>,
}

impl Peer {
    /// Create a new peer, set up its queues, and spawn queue pollers.
    pub async fn new(
        cla: Arc<ClaEntry>,
        peer_id: u32,
        cla_addr: ClaAddress,
        poll_channel_depth: usize,
        store: Arc<storage::Store>,
        dispatcher: Arc<Dispatcher>,
        tasks: &hardy_async::TaskPool,
    ) -> Self {
        let controller: Arc<dyn policy::EgressController> = cla
            .policy
            .new_controller(egress_queue::new_queue_set(
                cla.cla.clone(),
                dispatcher,
                peer_id,
                cla_addr,
                cla.cla.queue_count(),
            ))
            .await;

        let queue_count = cla.policy.queue_count();
        let mut queues = HashMap::with_capacity(queue_count as usize + 1);
        queues.insert(
            None,
            Self::start_queue_poller(
                poll_channel_depth,
                controller.clone(),
                store.clone(),
                tasks,
                peer_id,
                None,
            ),
        );

        for q in 0..queue_count {
            queues.insert(
                Some(q),
                Self::start_queue_poller(
                    poll_channel_depth,
                    controller.clone(),
                    store.clone(),
                    tasks,
                    peer_id,
                    Some(q),
                ),
            );
        }

        Self {
            cla: Arc::downgrade(&cla),
            queues,
        }
    }

    fn start_queue_poller(
        poll_channel_depth: usize,
        controller: Arc<dyn policy::EgressController>,
        store: Arc<storage::Store>,
        tasks: &hardy_async::TaskPool,
        peer: u32,
        queue: Option<u32>,
    ) -> storage::channel::Sender {
        let (tx, rx) = store.channel(
            bundle::BundleStatus::ForwardPending { peer, queue },
            poll_channel_depth,
        );

        hardy_async::spawn!(
            tasks,
            "egress_queue_poller",
            (peer = peer, queue = queue),
            async move {
                while let Ok(Some(bundle)) = rx.recv_async().await {
                    controller.forward(queue, bundle).await;
                }
            }
        );

        tx
    }

    pub async fn forward(
        &self,
        bundle: bundle::Bundle,
    ) -> core::result::Result<(), bundle::Bundle> {
        let queue = if let Some(flow_label) = bundle.metadata.writable.flow_label {
            let Some(cla) = self.cla.upgrade() else {
                return Err(bundle);
            };
            cla.policy.classify(Some(flow_label))
        } else {
            None
        };

        let queue = self
            .queues
            .get(&queue)
            .unwrap_or_else(|| self.queues.get(&None).trace_expect("No None queue?!?"));

        match queue.send(bundle).await {
            Ok(_) => Ok(()),
            Err(storage::channel::SendError(b)) => Err(b),
        }
    }

    async fn close(&self) {
        for tx in self.queues.values() {
            tx.close().await;
        }
    }
}

// PeerTable uses hardy_async::sync::spin::RwLock because:
// 1. All operations are O(1) HashMap lookups/inserts
// 2. Read-heavy pattern (forward is called frequently)
// 3. No blocking/iteration while holding lock
// 4. Avoids OS rwlock overhead on hot forwarding path

#[derive(Default)]
struct PeerTableInner {
    peers: HashMap<u32, Arc<Peer>>,
    next: u32,
}

pub struct PeerTable {
    inner: hardy_async::sync::spin::RwLock<PeerTableInner>,
}

impl PeerTable {
    pub fn new() -> Self {
        Self {
            inner: hardy_async::sync::spin::RwLock::new(PeerTableInner::default()),
        }
    }

    /// Reserve a peer_id without inserting a peer yet.
    pub fn reserve(&self) -> u32 {
        let mut inner = self.inner.write();
        loop {
            inner.next = inner.next.wrapping_add(1);
            if !inner.peers.contains_key(&inner.next) {
                return inner.next;
            }
        }
    }

    /// Activate a previously reserved peer_id with a fully initialized peer.
    pub fn activate(&self, peer_id: u32, peer: Arc<Peer>) {
        self.inner.write().peers.insert(peer_id, peer);
    }

    pub async fn remove(&self, peer_id: u32) {
        let peer = self.inner.write().peers.remove(&peer_id);

        if let Some(peer) = peer {
            peer.close().await;
        }
    }

    pub async fn forward(
        &self,
        peer_id: u32,
        bundle: bundle::Bundle,
    ) -> core::result::Result<(), bundle::Bundle> {
        let Some(peer) = self.inner.read().peers.get(&peer_id).cloned() else {
            return Err(bundle);
        };

        peer.forward(bundle).await
    }
}
