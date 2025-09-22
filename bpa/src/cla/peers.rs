use super::*;
use registry::Cla;
use std::{collections::HashMap, sync::RwLock};

struct Queue {
    task: tokio::task::JoinHandle<()>,
    notify: Arc<tokio::sync::Notify>,
}

pub struct Peer {
    cla: Arc<Cla>,
    queues: Vec<Queue>,
    cancel_token: tokio_util::sync::CancellationToken,
}

impl Peer {
    pub async fn new(peer: u32, cla: Arc<Cla>, store: &Arc<store::Store>) -> Self {
        let controller = cla.policy.new_controller(cla.cla.clone()).await;
        let cancel_token = tokio_util::sync::CancellationToken::new();

        let mut queues = Vec::new();
        for q in 0..cla.policy.queue_count() {
            queues.push(Self::start_queue_poller(peer, q, &cancel_token, store, &controller).await)
        }

        Self {
            cla,
            queues,
            cancel_token,
        }
    }

    async fn start_queue_poller(
        peer: u32,
        queue: u32,
        cancel_token: &tokio_util::sync::CancellationToken,
        store: &Arc<store::Store>,
        controller: &Arc<dyn EgressController>,
    ) -> Queue {
        // Tuning parameter
        const CHANNEL_DEPTH: usize = 16;

        let notify = Arc::new(tokio::sync::Notify::new());
        let notify_clone = notify.clone();
        let cancel_token = cancel_token.clone();
        let controller = controller.clone();

        let task = async move {
            let pending_state = metadata::BundleStatus::ForwardPending { peer, queue };
            loop {
                tokio::select!(
                    _ = notify_clone.notified() => {},
                    _ = cancel_token.cancelled() => {
                        break;
                    }
                );

                let outer_cancel_token = cancel_token.child_token();
                let cancel_token = outer_cancel_token.clone();
                let (tx, rx) = flume::bounded::<bundle::Bundle>(CHANNEL_DEPTH);
                let task = async move {
                    loop {
                        tokio::select! {
                            bundle = rx.recv_async() => {
                                let Ok(bundle) = bundle else {
                                    break;
                                };

                                if bundle.metadata.status == pending_state {



                                }
                            },
                            _ = cancel_token.cancelled() => {
                                break;
                            }
                        }
                    }
                };

                #[cfg(feature = "tracing")]
                let task = {
                    let span = tracing::trace_span!("parent: None", "refill_cache_reader");
                    span.follows_from(tracing::Span::current());
                    task.instrument(span)
                };

                let h = tokio::spawn(task);

                if store
                    .poll_pending(tx, pending_state, CHANNEL_DEPTH)
                    .await
                    .inspect_err(|e| error!("Failed to poll store for pending bundles: {e}"))
                    .is_err()
                {
                    // Cancel the reader task
                    outer_cancel_token.cancel();
                }

                _ = h.await;
            }
        };

        #[cfg(feature = "tracing")]
        let task = {
            let span = tracing::trace_span!("parent: None", "egress_queue_poller");
            span.follows_from(tracing::Span::current());
            task.instrument(span)
        };

        Queue {
            task: tokio::spawn(task),
            notify,
        }
    }

    pub fn map_peer_queue(&self, flow_label: u32) -> u32 {
        let queue = self.cla.policy.classify(flow_label);
        if let Some(q) = self.queues.get(queue as usize) {
            q.notify.notify_one();
        }
        queue
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        tokio::runtime::Handle::current().block_on(async move {
            self.cancel_token.cancel();

            for q in core::mem::take(&mut self.queues) {
                _ = q.task.await;
            }
        });
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

    pub fn map_peer_queue(&self, peer_id: u32, flow_label: u32) -> Option<u32> {
        let peer = self
            .inner
            .read()
            .trace_expect("Failed to lock mutex")
            .peers
            .get(&peer_id)
            .cloned()?;

        Some(peer.map_peer_queue(flow_label))
    }
}
