use super::*;
use registry::Cla;
use std::sync::{RwLock, Weak};

struct PeerInner {
    queues: HashMap<Option<u32>, storage::channel::Sender>,
}

pub struct Peer {
    cla: Weak<Cla>,
    inner: std::sync::OnceLock<PeerInner>,
}

impl Peer {
    pub fn new(cla: Weak<Cla>) -> Self {
        Self {
            cla,
            inner: std::sync::OnceLock::new(),
        }
    }

    pub async fn start(
        &self,
        poll_channel_depth: usize,
        cla: Arc<Cla>,
        peer: u32,
        cla_addr: ClaAddress,
        store: Arc<storage::Store>,
        dispatcher: Arc<dispatcher::Dispatcher>,
    ) {
        let controller = cla
            .policy
            .new_controller(egress_queue::new_queue_set(
                cla.cla.clone(),
                dispatcher,
                peer,
                cla_addr,
                cla.cla.queue_count(),
            ))
            .await;

        let mut queues = HashMap::new();
        queues.insert(
            None,
            Self::start_queue_poller(
                poll_channel_depth,
                controller.clone(),
                store.clone(),
                peer,
                None,
            ),
        );

        for q in 0..cla.policy.queue_count() {
            queues.insert(
                Some(q),
                Self::start_queue_poller(
                    poll_channel_depth,
                    controller.clone(),
                    store.clone(),
                    peer,
                    Some(q),
                ),
            );
        }

        self.inner.get_or_init(|| PeerInner { queues });
    }

    fn start_queue_poller(
        poll_channel_depth: usize,
        controller: Arc<dyn policy::EgressController>,
        store: Arc<storage::Store>,
        peer: u32,
        queue: Option<u32>,
    ) -> storage::channel::Sender {
        let (tx, rx) = store.channel(
            metadata::BundleStatus::ForwardPending { peer, queue },
            poll_channel_depth,
        );

        let task = async move {
            Self::poll_queue(controller, queue, rx).await;
        };

        #[cfg(feature = "tracing")]
        let task = {
            let span = tracing::trace_span!(
                parent: None,
                "egress_queue_poller",
                peer = peer,
                queue = queue
            );
            span.follows_from(tracing::Span::current());
            task.instrument(span)
        };

        tokio::spawn(task);

        tx
    }

    async fn poll_queue(
        controller: Arc<dyn policy::EgressController>,
        queue: Option<u32>,
        rx: storage::channel::Receiver,
    ) {
        while let Ok(bundle) = rx.recv_async().await {
            controller.forward(queue, bundle).await;
        }
    }

    pub async fn forward(
        &self,
        bundle: bundle::Bundle,
    ) -> core::result::Result<(), bundle::Bundle> {
        let queue = if let Some(flow_label) = bundle.metadata.flow_label {
            let Some(cla) = self.cla.upgrade() else {
                return Err(bundle);
            };
            cla.policy.classify(Some(flow_label))
        } else {
            None
        };

        let queues = &self.inner.wait().queues;
        let queue = queues
            .get(&queue)
            .unwrap_or_else(|| queues.get(&None).trace_expect("No None queue?!?"));

        match queue.send(bundle).await {
            Ok(_) => Ok(()),
            Err(storage::channel::SendError(b)) => Err(b),
        }
    }

    async fn close(&self) {
        for tx in self.inner.wait().queues.values() {
            tx.close().await;
        }
    }
}

#[derive(Default)]
struct PeerTableInner {
    peers: HashMap<u32, Arc<Peer>>,
    next: u32,
}

pub struct PeerTable {
    inner: RwLock<PeerTableInner>,
}

impl PeerTable {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(PeerTableInner::default()),
        }
    }

    pub fn insert(&self, peer: Arc<Peer>) -> u32 {
        let mut inner = self.inner.write().trace_expect("Failed to lock mutex");
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
        let peer = self
            .inner
            .write()
            .trace_expect("Failed to lock mutex")
            .peers
            .remove(&peer_id);

        if let Some(peer) = peer {
            peer.close().await;
        }
    }

    pub async fn forward(
        &self,
        peer_id: u32,
        bundle: bundle::Bundle,
    ) -> core::result::Result<(), bundle::Bundle> {
        let Some(peer) = self
            .inner
            .read()
            .trace_expect("Failed to lock mutex")
            .peers
            .get(&peer_id)
            .cloned()
        else {
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
