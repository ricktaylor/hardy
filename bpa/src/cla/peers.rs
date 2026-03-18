use trace_err::TraceErrOption;

use super::{ClaAddress, ClaRecord};
use crate::bundle::{Bundle, BundleStatus};
use crate::dispatcher::Dispatcher;
use crate::policy::EgressController;
use crate::storage::{
    Store,
    channel::{SendError, Sender},
};
use crate::{Arc, HashMap, Weak};

// PeerTable uses hardy_async::sync::spin::RwLock because:
// 1. All operations are O(1) HashMap lookups/inserts
// 2. Read-heavy pattern (forward is called frequently)
// 3. No blocking/iteration while holding lock
// 4. Avoids OS rwlock overhead on hot forwarding path

struct PeerInner {
    queues: HashMap<Option<u32>, Sender>,
}

pub struct Peer {
    cla: Weak<ClaRecord>,
    inner: hardy_async::sync::spin::Once<PeerInner>,
}

impl Peer {
    pub fn new(cla: Weak<ClaRecord>) -> Self {
        Self {
            cla,
            inner: hardy_async::sync::spin::Once::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn start(
        &self,
        poll_channel_depth: usize,
        cla: Arc<ClaRecord>,
        peer: u32,
        cla_addr: ClaAddress,
        store: Arc<Store>,
        dispatcher: Arc<Dispatcher>,
        tasks: &hardy_async::TaskPool,
    ) {
        let controller = cla
            .policy
            .new_controller(super::new_queue_set(
                cla.cla.clone(),
                dispatcher,
                peer,
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
                peer,
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
                    peer,
                    Some(q),
                ),
            );
        }

        self.inner.call_once(|| PeerInner { queues });
    }

    fn start_queue_poller(
        poll_channel_depth: usize,
        controller: Arc<dyn EgressController>,
        store: Arc<Store>,
        tasks: &hardy_async::TaskPool,
        peer: u32,
        queue: Option<u32>,
    ) -> Sender {
        let (tx, rx) = store.channel(
            BundleStatus::ForwardPending { peer, queue },
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

    pub async fn forward(&self, bundle: Bundle) -> core::result::Result<(), Bundle> {
        let queue = if let Some(flow_label) = bundle.metadata.writable.flow_label {
            let Some(cla) = self.cla.upgrade() else {
                return Err(bundle);
            };
            cla.policy.classify(Some(flow_label))
        } else {
            None
        };

        let queues = match self.inner.get() {
            Some(inner) => &inner.queues,
            None => return Err(bundle),
        };
        let queue = queues
            .get(&queue)
            .unwrap_or_else(|| queues.get(&None).trace_expect("No None queue?!?"));

        match queue.send(bundle).await {
            Ok(_) => Ok(()),
            Err(SendError(b)) => Err(b),
        }
    }

    async fn close(&self) {
        if let Some(inner) = self.inner.get() {
            for tx in inner.queues.values() {
                tx.close().await;
            }
        }
    }
}

#[derive(Default)]
struct PeerTableInner {
    peers: HashMap<u32, Arc<Peer>>,
    next: u32,
}

pub struct PeerTable {
    inner: hardy_async::sync::spin::RwLock<PeerTableInner>,
}

impl Default for PeerTable {
    fn default() -> Self {
        Self::new()
    }
}

impl PeerTable {
    pub fn new() -> Self {
        Self {
            inner: hardy_async::sync::spin::RwLock::new(PeerTableInner::default()),
        }
    }

    pub fn insert(&self, peer: Arc<Peer>) -> u32 {
        // sync::spin::RwLock::write() returns guard directly (no Result)
        let mut inner = self.inner.write();
        let peer_id = loop {
            inner.next = inner.next.wrapping_add(1);
            if !inner.peers.contains_key(&inner.next) {
                break inner.next;
            }
        };

        inner.peers.insert(peer_id, peer);
        peer_id
    }

    pub async fn remove(&self, peer_id: u32) {
        let peer = self.inner.write().peers.remove(&peer_id);

        if let Some(peer) = peer {
            peer.close().await;
        }
    }

    pub async fn forward(&self, peer_id: u32, bundle: Bundle) -> core::result::Result<(), Bundle> {
        // sync::spin::RwLock::read() returns guard directly (no Result)
        let Some(peer) = self.inner.read().peers.get(&peer_id).cloned() else {
            return Err(bundle);
        };

        peer.forward(bundle).await
    }
}

#[cfg(test)]
mod tests {
    // use super::*;

    // // TODO: Implement test for 'Queue Selection' (Verify Policy maps to correct CLA queue)
    // #[test]
    // fn test_queue_selection() {
    //     todo!("Verify Policy maps to correct CLA queue");
    // }

    // // TODO: Implement test for 'Queue Fallback' (Verify fallback to default queue on invalid index)
    // #[test]
    // fn test_queue_fallback() {
    //     todo!("Verify fallback to default queue on invalid index");
    // }
}
