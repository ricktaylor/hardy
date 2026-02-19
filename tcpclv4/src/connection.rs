use super::*;
use rand::seq::IteratorRandom;
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    sync::{Arc, Mutex},
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
}

impl ConnectionPool {
    fn new(
        conn: Connection,
        sink: Arc<dyn hardy_bpa::cla::Sink>,
        remote_addr: SocketAddr,
        max_idle: usize,
    ) -> Self {
        Self {
            inner: Mutex::new(ConnectionPoolInner {
                active: HashMap::new(),
                idle: vec![conn],
                peers: HashSet::new(),
            }),
            sink,
            max_idle,
            remote_addr: hardy_bpa::cla::ClaAddress::Tcp(remote_addr),
        }
    }

    fn add(&self, conn: Connection) {
        self.inner
            .lock()
            .trace_expect("Failed to lock mutex")
            .idle
            .push(conn);
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    async fn add_peer(&self, node_id: NodeId) {
        if self
            .inner
            .lock()
            .trace_expect("Failed to lock mutex")
            .peers
            .insert(node_id.clone())
            && !self
                .sink
                .add_peer(node_id.clone(), self.remote_addr.clone())
                .await
                .unwrap_or_else(|e| {
                    error!("add_peer failed: {e:?}");
                    false
                })
        {
            self.inner
                .lock()
                .trace_expect("Failed to lock mutex")
                .peers
                .remove(&node_id);
        }
    }

    async fn remove(&self, local_addr: &SocketAddr) -> bool {
        let peers = {
            let mut inner = self.inner.lock().trace_expect("Failed to lock mutex");
            inner.active.remove(local_addr);
            inner.idle.retain(|c| &c.local_addr != local_addr);

            if inner.active.is_empty() && inner.idle.is_empty() {
                Some(std::mem::take(&mut inner.peers))
            } else {
                None
            }
        };

        if let Some(peers) = peers {
            for p in peers {
                _ = self.sink.remove_peer(p, &self.remote_addr).await;
            }
            true
        } else {
            false
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self, bundle)))]
    async fn try_send(
        &self,
        bundle: hardy_bpa::Bytes,
    ) -> Result<hardy_bpa::cla::ForwardBundleResult, hardy_bpa::Bytes> {
        // We repeatedly search as this function is async, so changes can happen while running
        loop {
            // Try to use an idle session
            while let Some(conn) = {
                let mut inner = self.inner.lock().trace_expect("Failed to lock mutex");
                let conn = inner.idle.pop();
                if let Some(conn) = &conn {
                    inner.active.insert(conn.local_addr, conn.tx.clone());
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

            if self.max_idle == 0 || {
                let inner = self.inner.lock().trace_expect("Failed to lock mutex");
                inner.active.len() + inner.idle.len()
            } <= self.max_idle
            {
                // We can support more active connections
                return Err(bundle);
            }

            // Pick a random active connection and enqueue
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
        }
    }
}

pub struct ConnectionRegistry {
    pools: Mutex<HashMap<SocketAddr, Arc<connection::ConnectionPool>>>,
    max_idle: usize,
}

impl ConnectionRegistry {
    pub fn new(max_idle: usize) -> Self {
        Self {
            pools: Mutex::new(HashMap::new()),
            max_idle,
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    pub async fn shutdown(&self) {
        // Closing tx channels causes session::run tasks to exit
        self.pools
            .lock()
            .trace_expect("Failed to lock mutex")
            .clear();
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self, sink, conn)))]
    pub async fn register_session(
        &self,
        sink: Arc<dyn hardy_bpa::cla::Sink>,
        conn: Connection,
        remote_addr: SocketAddr,
        node_id: Option<NodeId>,
    ) {
        let pool = match self
            .pools
            .lock()
            .trace_expect("Failed to lock mutex")
            .entry(remote_addr)
        {
            std::collections::hash_map::Entry::Occupied(mut e) => {
                let pool = e.get_mut();
                pool.add(conn);
                pool.clone()
            }
            std::collections::hash_map::Entry::Vacant(e) => e
                .insert(Arc::new(connection::ConnectionPool::new(
                    conn,
                    sink,
                    remote_addr,
                    self.max_idle,
                )))
                .clone(),
        };

        if let Some(node_id) = node_id {
            pool.add_peer(node_id).await
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    pub async fn unregister_session(&self, local_addr: &SocketAddr, remote_addr: &SocketAddr) {
        let pool = self
            .pools
            .lock()
            .trace_expect("Failed to lock mutex")
            .get(remote_addr)
            .cloned();

        if let Some(pool) = pool
            && pool.remove(local_addr).await
        {
            self.pools
                .lock()
                .trace_expect("Failed to lock mutex")
                .remove(remote_addr);
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self, bundle)))]
    pub async fn forward(
        &self,
        remote_addr: &SocketAddr,
        mut bundle: hardy_bpa::Bytes,
    ) -> Result<hardy_bpa::cla::ForwardBundleResult, hardy_bpa::Bytes> {
        let pool = self
            .pools
            .lock()
            .trace_expect("Failed to lock mutex")
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
