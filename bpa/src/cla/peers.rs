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
            Self::start_queue_poller(controller.clone(), store.clone(), peer, None),
        );

        for q in 0..cla.policy.queue_count() {
            queues.insert(
                Some(q),
                Self::start_queue_poller(controller.clone(), store.clone(), peer, Some(q)),
            );
        }

        self.inner.get_or_init(|| PeerInner { queues });
    }

    fn start_queue_poller(
        controller: Arc<dyn policy::EgressController>,
        store: Arc<storage::Store>,
        peer: u32,
        queue: Option<u32>,
    ) -> storage::channel::Sender {
        let (tx, rx) = store.channel(metadata::BundleStatus::ForwardPending { peer, queue }, 16);

        let task = async move {
            Self::poll_queue(controller, queue, rx).await;
        };

        #[cfg(feature = "tracing")]
        let task = {
            let span = tracing::trace_span!("parent: None", "egress_queue_poller", peer, queue);
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
        loop {
            let Ok(bundle) = rx.recv_async().await else {
                break;
            };

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

        let queues = &self.inner.get().trace_expect("No queues?").queues;
        let queue = queues
            .get(&queue)
            .unwrap_or_else(|| queues.get(&None).trace_expect("No None queue?!?"));

        match queue.send(bundle).await {
            Ok(_) => Ok(()),
            Err(storage::channel::SendError(b)) => Err(b),
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

    pub fn remove(&self, peer_id: u32) -> Option<Arc<Peer>> {
        self.inner
            .write()
            .trace_expect("Failed to lock mutex")
            .peers
            .remove(&peer_id)
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
