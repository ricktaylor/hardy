use super::*;
use registry::Cla;
use std::{collections::HashMap, sync::RwLock};

struct Queue {
    task: tokio::task::JoinHandle<()>,
    notify: Arc<tokio::sync::Notify>,
}

struct PollArgs {
    peer: u32,
    addr: ClaAddress,
    store: Arc<store::Store>,
    controller: Arc<dyn EgressController>,
    dispatcher: Arc<dispatcher::Dispatcher>,
}

struct PeerInner {
    queues: Vec<Queue>,
    cancel_token: tokio_util::sync::CancellationToken,
}

pub struct Peer {
    cla: Arc<Cla>,
    inner: std::sync::OnceLock<PeerInner>,
}

impl Peer {
    pub fn new(cla: Arc<Cla>) -> Self {
        Self {
            cla,
            inner: std::sync::OnceLock::new(),
        }
    }

    pub async fn start(
        &self,
        peer: u32,
        addr: ClaAddress,
        store: Arc<store::Store>,
        dispatcher: Arc<dispatcher::Dispatcher>,
    ) {
        let controller = self.cla.policy.new_controller(self.cla.cla.clone()).await;
        let cancel_token = tokio_util::sync::CancellationToken::new();

        let mut queues = Vec::new();
        let args = Arc::new(PollArgs {
            peer,
            addr,
            store,
            controller,
            dispatcher,
        });

        for q in 0..self.cla.policy.queue_count() {
            queues.push(Self::start_queue_poller(q, args.clone(), cancel_token.clone()).await)
        }

        self.inner.get_or_init(|| PeerInner {
            queues,
            cancel_token,
        });
    }

    async fn start_queue_poller(
        queue: u32,
        args: Arc<PollArgs>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Queue {
        let notify = Arc::new(tokio::sync::Notify::new());
        let notify_clone = notify.clone();

        let task = async move { Self::poll_queue(queue, args, notify_clone, cancel_token).await };

        #[cfg(feature = "tracing")]
        let task = {
            let span = tracing::trace_span!("parent: None", "egress_queue_poller", peer, queue);
            span.follows_from(tracing::Span::current());
            task.instrument(span)
        };

        Queue {
            task: tokio::spawn(task),
            notify,
        }
    }

    async fn poll_queue(
        queue: u32,
        args: Arc<PollArgs>,
        notify: Arc<tokio::sync::Notify>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        // Tuning parameter
        const CHANNEL_DEPTH: usize = 16;

        loop {
            tokio::select!(
                _ = notify.notified() => {},
                _ = cancel_token.cancelled() => {
                    break;
                }
            );

            // Keep calling while we can dequeue items
            loop {
                let outer_cancel_token = cancel_token.child_token();
                let cancel_token = outer_cancel_token.clone();
                let args_clone = args.clone();
                let (tx, rx) = flume::bounded::<bundle::Bundle>(CHANNEL_DEPTH);
                let task = async move {
                    let mut got_some = false;
                    loop {
                        tokio::select! {
                            bundle = rx.recv_async() => {
                                let Ok(bundle) = bundle else {
                                    break got_some;
                                };

                                if bundle.metadata.status == (metadata::BundleStatus::ForwardPending { peer: args_clone.peer, queue }) {
                                    match args_clone.dispatcher.forward_bundle(bundle,args_clone.controller.as_ref(),queue,args_clone.addr.clone()).await {
                                        Ok(ForwardBundleResult::Sent) => {},
                                        Ok(ForwardBundleResult::NoNeighbour) => {
                                            // The neighbour has gone, kill the queue
                                            trace!("CLA indicates neighbour has gone, clearing queue assignment for peer {}",args_clone.peer);

                                            if let Err(e) = args_clone.store.reset_peer_queue(args_clone.peer).await {
                                                error!("Failed to reset peer queue: {e}");
                                            }

                                            // Don't loop further, the peer has gone
                                            break false;
                                        },
                                        Err(e) => {
                                            error!("Failed to forward bundle: {e}");
                                            break false;
                                        }
                                    }
                                }
                                got_some = true;
                            },
                            _ = cancel_token.cancelled() => {
                                break false;
                            }
                        }
                    }
                };

                #[cfg(feature = "tracing")]
                let task = {
                    let span =
                        tracing::trace_span!("parent: None", "poll_queue_reader", peer, queue);
                    span.follows_from(tracing::Span::current());
                    task.instrument(span)
                };

                let h = tokio::spawn(task);

                if args
                    .store
                    .poll_pending(
                        tx,
                        &metadata::BundleStatus::ForwardPending {
                            peer: args.peer,
                            queue,
                        },
                        CHANNEL_DEPTH,
                    )
                    .await
                    .inspect_err(|e| error!("Failed to poll store for pending bundles: {e}"))
                    .is_err()
                {
                    // Cancel the reader task
                    outer_cancel_token.cancel();
                }

                if !matches!(h.await, Ok(true)) {
                    break;
                }
            }
        }
    }

    pub fn map_peer_queue(&self, flow_label: u32) -> u32 {
        let queue = self.cla.policy.classify(flow_label);
        if let Some(q) = self
            .inner
            .get()
            .and_then(|inner| inner.queues.get(queue as usize))
        {
            q.notify.notify_one();
        }
        queue
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.get_mut() {
            tokio::runtime::Handle::current().block_on(async move {
                inner.cancel_token.cancel();
                for q in core::mem::take(&mut inner.queues) {
                    _ = q.task.await;
                }
            });
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
