use super::*;
use rand::seq::IteratorRandom;
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    slice,
    sync::{Arc, Mutex, RwLock},
};

pub type ConnectionTx = tokio::sync::mpsc::Sender<(
    hardy_bpa::Bytes,
    tokio::sync::oneshot::Sender<hardy_bpa::cla::ForwardBundleResult>,
)>;

pub struct Connection {
    pub tx: ConnectionTx,
    pub local_addr: SocketAddr,
}

struct ConnectionPoolInner {
    active: HashMap<SocketAddr, ConnectionTx>,
    idle: Vec<Connection>,
    peers: HashSet<NodeId>,
}

struct ConnectionPool {
    inner: Mutex<ConnectionPoolInner>,
    sink: Arc<dyn hardy_bpa::cla::Sink>,
    max_idle: usize,
    remote_addr: hardy_bpa::cla::ClaAddress,
    socket_addr: SocketAddr,
    node_to_addr: Arc<RwLock<HashMap<NodeId, SocketAddr>>>,
}

impl ConnectionPool {
    fn new(
        conn: Connection,
        sink: Arc<dyn hardy_bpa::cla::Sink>,
        remote_addr: SocketAddr,
        max_idle: usize,
        node_to_addr: Arc<RwLock<HashMap<NodeId, SocketAddr>>>,
    ) -> Self {
        metrics::gauge!("tcpclv4.pool.idle").increment(1.0);
        Self {
            inner: Mutex::new(ConnectionPoolInner {
                active: HashMap::new(),
                idle: vec![conn],
                peers: HashSet::new(),
            }),
            sink,
            max_idle,
            remote_addr: hardy_bpa::cla::ClaAddress::Tcp(remote_addr),
            socket_addr: remote_addr,
            node_to_addr,
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
        if self
            .inner
            .lock()
            .trace_expect("Failed to lock mutex")
            .peers
            .insert(node_id.clone())
        {
            if self
                .sink
                .add_peer(self.remote_addr.clone(), slice::from_ref(&node_id))
                .await
                .unwrap_or_else(|e| {
                    warn!("add_peer failed: {e:?}");
                    false
                })
            {
                // Record NodeId → SocketAddr mapping only after sink confirms
                self.node_to_addr
                    .write()
                    .trace_expect("Failed to lock rwlock")
                    .insert(node_id, self.socket_addr);
            } else {
                self.inner
                    .lock()
                    .trace_expect("Failed to lock mutex")
                    .peers
                    .remove(&node_id);
            }
        }
    }

    async fn remove(&self, local_addr: &SocketAddr) -> bool {
        let remove_addr = {
            let mut inner = self.inner.lock().trace_expect("Failed to lock mutex");
            inner.active.remove(local_addr);
            let before = inner.idle.len();
            inner.idle.retain(|c| &c.local_addr != local_addr);
            let removed = before - inner.idle.len();
            if removed > 0 {
                metrics::gauge!("tcpclv4.pool.idle").decrement(removed as f64);
            }

            inner.active.is_empty() && inner.idle.is_empty()
        };

        if remove_addr {
            // Remove all NodeId → SocketAddr mappings for this pool's peers
            let peers: Vec<NodeId> = self
                .inner
                .lock()
                .trace_expect("Failed to lock mutex")
                .peers
                .drain()
                .collect();
            {
                let mut map = self
                    .node_to_addr
                    .write()
                    .trace_expect("Failed to lock rwlock");
                for peer in &peers {
                    map.remove(peer);
                }
            }
            _ = self.sink.remove_peer(&self.remote_addr).await;
            true
        } else {
            false
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, bundle)))]
    async fn try_send(
        &self,
        bundle: hardy_bpa::Bytes,
    ) -> Result<hardy_bpa::cla::ForwardBundleResult, hardy_bpa::Bytes> {
        // Retry loop with a limit to prevent spinning when all connections fail
        const MAX_RETRIES: usize = 3;
        let mut retries = 0;
        loop {
            // Try to use an idle session
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

                // By the time we got here, conn is in a bad state
                self.inner
                    .lock()
                    .trace_expect("Failed to lock mutex")
                    .active
                    .remove(&conn.local_addr);
            }

            // Try sending via an active connection before giving up
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

                // By the time we got here, conn is in a bad state
                self.inner
                    .lock()
                    .trace_expect("Failed to lock mutex")
                    .active
                    .remove(&local_addr);
            }

            // No idle or active connections could send — tell caller to open a new one
            // if the pool has capacity, otherwise retry
            if self.max_idle == 0 || {
                let inner = self.inner.lock().trace_expect("Failed to lock mutex");
                inner.active.len() + inner.idle.len()
            } <= self.max_idle
            {
                return Err(bundle);
            }

            retries += 1;
            if retries >= MAX_RETRIES {
                return Err(bundle);
            }
            tokio::task::yield_now().await;
        }
    }
}

pub struct ConnectionRegistry {
    pools: RwLock<HashMap<SocketAddr, Arc<ConnectionPool>>>,
    node_to_addr: Arc<RwLock<HashMap<NodeId, SocketAddr>>>,
    max_idle: usize,
}

impl ConnectionRegistry {
    pub fn new(max_idle: usize) -> Self {
        Self {
            pools: RwLock::new(HashMap::new()),
            node_to_addr: Arc::new(RwLock::new(HashMap::new())),
            max_idle,
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    pub fn shutdown(&self) {
        let mut pools = self.pools.write().trace_expect("Failed to lock rwlock");

        // Count remaining idle connections before clearing
        let idle: usize = pools.values().map(|pool| pool.idle_count()).sum();
        if idle > 0 {
            metrics::gauge!("tcpclv4.pool.idle").decrement(idle as f64);
        }

        // Closing tx channels causes session::run tasks to exit
        pools.clear();
    }

    /// Resolve a next-hop EID to a SocketAddr for forwarding.
    /// Returns None if the peer is unknown or the connection pool no longer exists.
    pub fn resolve_next_hop(&self, next_hop: &hardy_bpv7::eid::Eid) -> Option<SocketAddr> {
        let node_id = next_hop.to_node_id().ok()?;
        let socket_addr = self
            .node_to_addr
            .read()
            .trace_expect("Failed to lock rwlock")
            .get(&node_id)
            .copied()?;

        // Validate the pool still exists for this address
        if self
            .pools
            .read()
            .trace_expect("Failed to lock rwlock")
            .contains_key(&socket_addr)
        {
            Some(socket_addr)
        } else {
            // Stale mapping — pool was removed
            self.node_to_addr
                .write()
                .trace_expect("Failed to lock rwlock")
                .remove(&node_id);
            None
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, sink, conn)))]
    pub async fn register_session(
        &self,
        sink: Arc<dyn hardy_bpa::cla::Sink>,
        conn: Connection,
        remote_addr: SocketAddr,
        node_id: Option<NodeId>,
    ) {
        let pool = match self
            .pools
            .write()
            .trace_expect("Failed to lock rwlock")
            .entry(remote_addr)
        {
            std::collections::hash_map::Entry::Occupied(mut e) => {
                let pool = e.get_mut();
                pool.add(conn);
                pool.clone()
            }
            std::collections::hash_map::Entry::Vacant(e) => e
                .insert(Arc::new(ConnectionPool::new(
                    conn,
                    sink,
                    remote_addr,
                    self.max_idle,
                    self.node_to_addr.clone(),
                )))
                .clone(),
        };

        if let Some(node_id) = node_id {
            pool.add_peer(node_id).await
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    pub async fn unregister_session(&self, local_addr: &SocketAddr, remote_addr: &SocketAddr) {
        let pool = self
            .pools
            .read()
            .trace_expect("Failed to lock rwlock")
            .get(remote_addr)
            .cloned();

        if let Some(pool) = pool
            && pool.remove(local_addr).await
        {
            // Only remove if the pool in the map is still the same instance
            // (register_session may have replaced it between the two locks)
            let mut pools = self.pools.write().trace_expect("Failed to lock rwlock");
            if let Some(current) = pools.get(remote_addr) {
                if Arc::ptr_eq(current, &pool) {
                    pools.remove(remote_addr);
                }
            }
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, bundle)))]
    pub async fn forward(
        &self,
        remote_addr: &SocketAddr,
        mut bundle: hardy_bpa::Bytes,
    ) -> Result<hardy_bpa::cla::ForwardBundleResult, hardy_bpa::Bytes> {
        let pool = self
            .pools
            .read()
            .trace_expect("Failed to lock rwlock")
            .get(remote_addr)
            .cloned();

        if let Some(pool) = pool {
            match pool.try_send(bundle).await {
                Ok(r) => return Ok(r),
                Err(b) => {
                    bundle = b;
                }
            }
        }
        Err(bundle)
    }
}
