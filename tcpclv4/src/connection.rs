use super::*;
use rand::seq::IteratorRandom;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

pub type ConnectionTx = tokio::sync::mpsc::Sender<(
    hardy_bpa::Bytes,
    tokio::sync::oneshot::Sender<hardy_bpa::cla::ForwardBundleResult>,
)>;

pub struct Connection {
    pub tx: ConnectionTx,
    pub local_addr: SocketAddr,
}

struct PoolInner {
    active: HashMap<SocketAddr, ConnectionTx>,
    idle: Vec<Connection>,
    peers: HashSet<NodeId>,
}

struct ConnectionPool {
    inner: Mutex<PoolInner>,
    sink: Arc<dyn hardy_bpa::cla::Sink>,
    max_idle: usize,
    remote_addr: hardy_bpa::cla::ClaAddress,
}

impl ConnectionPool {
    fn new(
        conn: Connection,
        sink: Arc<dyn hardy_bpa::cla::Sink>,
        remote_addr: SocketAddr,
        max_idle: usize,
    ) -> Self {
        metrics::gauge!("tcpclv4.pool.idle").increment(1.0);
        Self {
            inner: Mutex::new(PoolInner {
                active: HashMap::new(),
                idle: vec![conn],
                peers: HashSet::new(),
            }),
            sink,
            max_idle,
            remote_addr: hardy_bpa::cla::ClaAddress::Tcp(remote_addr),
        }
    }

    fn idle_count(&self) -> usize {
        self.inner
            .lock()
            .trace_expect("Failed to lock mutex")
            .idle
            .len()
    }

    fn add(&self, conn: Connection) {
        self.inner
            .lock()
            .trace_expect("Failed to lock mutex")
            .idle
            .push(conn);
        metrics::gauge!("tcpclv4.pool.idle").increment(1.0);
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    async fn add_peer(&self, node_id: NodeId) {
        let inserted = self
            .inner
            .lock()
            .trace_expect("Failed to lock mutex")
            .peers
            .insert(node_id.clone());

        if !inserted {
            return;
        }

        let accepted = self
            .sink
            .add_peer(self.remote_addr.clone(), core::slice::from_ref(&node_id))
            .await
            .unwrap_or_else(|e| {
                warn!("add_peer failed: {e:?}");
                false
            });

        if !accepted {
            self.inner
                .lock()
                .trace_expect("Failed to lock mutex")
                .peers
                .remove(&node_id);
        }
    }

    async fn remove(&self, local_addr: &SocketAddr) -> bool {
        let empty = {
            let mut inner = self.inner.lock().trace_expect("Failed to lock mutex");
            inner.active.remove(local_addr);

            let before = inner.idle.len();
            inner.idle.retain(|c| c.local_addr != *local_addr);
            let removed = before - inner.idle.len();
            if removed > 0 {
                metrics::gauge!("tcpclv4.pool.idle").decrement(removed as f64);
            }

            let empty = inner.active.is_empty() && inner.idle.is_empty();
            if empty {
                inner.peers.clear();
            }
            empty
        };

        if empty {
            _ = self.sink.remove_peer(&self.remote_addr).await;
        }
        empty
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, bundle)))]
    async fn try_send(
        &self,
        bundle: hardy_bpa::Bytes,
    ) -> Result<hardy_bpa::cla::ForwardBundleResult, hardy_bpa::Bytes> {
        loop {
            // Try idle connections first
            while let Some(conn) = {
                let mut inner = self.inner.lock().trace_expect("Failed to lock mutex");
                let conn = inner.idle.pop();
                if let Some(conn) = &conn {
                    inner.active.insert(conn.local_addr, conn.tx.clone());
                    metrics::gauge!("tcpclv4.pool.idle").decrement(1.0);
                    metrics::counter!("tcpclv4.pool.reused").increment(1);
                }
                conn
            } {
                let (tx, rx) = tokio::sync::oneshot::channel();
                if conn.tx.send((bundle.clone(), tx)).await.is_ok() {
                    if let Ok(r) = rx.await {
                        let mut inner = self.inner.lock().trace_expect("Failed to lock mutex");
                        inner.active.remove(&conn.local_addr);
                        if inner.idle.len() + inner.active.len() <= self.max_idle {
                            inner.idle.push(conn);
                            metrics::gauge!("tcpclv4.pool.idle").increment(1.0);
                        }
                        return Ok(r);
                    }
                    debug!("Connection failed to transfer bundle");
                }

                self.inner
                    .lock()
                    .trace_expect("Failed to lock mutex")
                    .active
                    .remove(&conn.local_addr);
            }

            // Try a random active connection
            while let Some((local_addr, conn_tx)) = {
                self.inner
                    .lock()
                    .trace_expect("Failed to lock mutex")
                    .active
                    .iter()
                    .choose(&mut rand::rng())
                    .map(|(l, c)| (*l, c.clone()))
            } {
                let (tx, rx) = tokio::sync::oneshot::channel();
                if conn_tx.send((bundle.clone(), tx)).await.is_ok() {
                    if let Ok(r) = rx.await {
                        return Ok(r);
                    }
                    debug!("Connection failed to transfer bundle");
                }

                self.inner
                    .lock()
                    .trace_expect("Failed to lock mutex")
                    .active
                    .remove(&local_addr);
            }

            // No connections worked; allow caller to create a new one if under limit
            let total = {
                let inner = self.inner.lock().trace_expect("Failed to lock mutex");
                inner.active.len() + inner.idle.len()
            };
            if self.max_idle == 0 || total <= self.max_idle {
                return Err(bundle);
            }
        }
    }
}

struct RegistryInner {
    pools: HashMap<SocketAddr, Arc<ConnectionPool>>,
    known_peers: HashMap<NodeId, SocketAddr>,
}

pub struct ConnectionRegistry {
    inner: Mutex<RegistryInner>,
    max_idle: usize,
}

impl ConnectionRegistry {
    pub fn new(max_idle: usize) -> Self {
        Self {
            inner: Mutex::new(RegistryInner {
                pools: HashMap::new(),
                known_peers: HashMap::new(),
            }),
            max_idle,
        }
    }

    pub fn has_pool(&self, addr: &SocketAddr) -> bool {
        self.inner
            .lock()
            .trace_expect("Failed to lock mutex")
            .pools
            .contains_key(addr)
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    pub fn shutdown(&self) {
        let mut inner = self.inner.lock().trace_expect("Failed to lock mutex");

        let idle: usize = inner.pools.values().map(|pool| pool.idle_count()).sum();
        if idle > 0 {
            metrics::gauge!("tcpclv4.pool.idle").decrement(idle as f64);
        }

        inner.pools.clear();
        inner.known_peers.clear();
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, sink, conn)))]
    pub async fn register_session(
        &self,
        sink: Arc<dyn hardy_bpa::cla::Sink>,
        conn: Connection,
        remote_addr: SocketAddr,
        node_id: Option<NodeId>,
    ) {
        let (pool, new_peer) = {
            let mut inner = self.inner.lock().trace_expect("Failed to lock mutex");

            let pool = match inner.pools.entry(remote_addr) {
                std::collections::hash_map::Entry::Occupied(e) => {
                    let pool = e.into_mut();
                    pool.add(conn);
                    pool.clone()
                }
                std::collections::hash_map::Entry::Vacant(e) => e
                    .insert(Arc::new(ConnectionPool::new(
                        conn,
                        sink,
                        remote_addr,
                        self.max_idle,
                    )))
                    .clone(),
            };

            let new_peer = node_id.filter(|id| {
                if inner.known_peers.contains_key(id) {
                    debug!("Peer {id} already connected, skipping duplicate registration");
                    false
                } else {
                    inner.known_peers.insert(id.clone(), remote_addr);
                    true
                }
            });

            (pool, new_peer)
        };

        if let Some(node_id) = new_peer {
            pool.add_peer(node_id).await;
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    pub async fn unregister_session(&self, local_addr: &SocketAddr, remote_addr: &SocketAddr) {
        let pool = self
            .inner
            .lock()
            .trace_expect("Failed to lock mutex")
            .pools
            .get(remote_addr)
            .cloned();

        let Some(pool) = pool else { return };

        if !pool.remove(local_addr).await {
            return;
        }

        let mut inner = self.inner.lock().trace_expect("Failed to lock mutex");
        if let Some(current) = inner.pools.get(remote_addr) {
            if Arc::ptr_eq(current, &pool) {
                inner.pools.remove(remote_addr);
                inner.known_peers.retain(|_, addr| addr != remote_addr);
            }
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, bundle)))]
    pub async fn forward(
        &self,
        remote_addr: &SocketAddr,
        bundle: hardy_bpa::Bytes,
    ) -> Result<hardy_bpa::cla::ForwardBundleResult, hardy_bpa::Bytes> {
        let pool = self
            .inner
            .lock()
            .trace_expect("Failed to lock mutex")
            .pools
            .get(remote_addr)
            .cloned();

        match pool {
            Some(pool) => pool.try_send(bundle).await,
            None => Err(bundle),
        }
    }
}
