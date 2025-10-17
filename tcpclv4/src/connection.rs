use super::*;
use rand::seq::IteratorRandom;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

pub type ConnectionTx = tokio::sync::mpsc::Sender<(
    hardy_bpa::Bytes,
    tokio::sync::oneshot::Sender<Result<hardy_bpa::cla::ForwardBundleResult, hardy_bpa::Bytes>>,
)>;

pub struct Connection {
    pub tx: ConnectionTx,
    pub local_addr: SocketAddr,
}

struct ConnectionPoolInner {
    active: HashMap<SocketAddr, ConnectionTx>,
    idle: Vec<Connection>,
}

struct ConnectionPool {
    inner: Mutex<ConnectionPoolInner>,
    max_idle: usize,
}

impl ConnectionPool {
    fn new(conn: Connection, max_idle: usize) -> Self {
        Self {
            inner: Mutex::new(ConnectionPoolInner {
                active: HashMap::new(),
                idle: vec![conn],
            }),
            max_idle,
        }
    }

    fn add(&self, conn: Connection) {
        self.inner
            .lock()
            .trace_expect("Failed to lock mutex")
            .idle
            .push(conn);
    }

    fn remove(&self, local_addr: &SocketAddr) -> bool {
        let mut inner = self.inner.lock().trace_expect("Failed to lock mutex");
        if inner.active.remove(local_addr).is_none() {
            _ = inner.idle.extract_if(.., |c| &c.local_addr == local_addr);
        }
        inner.active.is_empty() && inner.idle.is_empty()
    }

    async fn try_send(
        &self,
        mut bundle: hardy_bpa::Bytes,
    ) -> Result<hardy_bpa::cla::Result<hardy_bpa::cla::ForwardBundleResult>, hardy_bpa::Bytes> {
        // We repeatedly search as this function is async, so changes can happen while running
        loop {
            // Try to use an idle session
            while let Some(conn) = {
                let mut inner = self.inner.lock().trace_expect("Failed to lock mutex");
                if let Some(conn) = inner.idle.pop() {
                    inner.active.insert(conn.local_addr, conn.tx.clone());
                    Some(conn)
                } else {
                    None
                }
            } {
                let (tx, rx) = tokio::sync::oneshot::channel();
                if let Err(e) = conn.tx.send((bundle, tx)).await {
                    self.inner
                        .lock()
                        .trace_expect("Failed to lock mutex")
                        .active
                        .remove(&conn.local_addr);
                    bundle = e.0.0;
                } else {
                    match rx.await.trace_expect("Sender dropped!") {
                        Ok(r) => {
                            let mut inner = self.inner.lock().trace_expect("Failed to lock mutex");
                            inner.active.remove(&conn.local_addr);
                            if inner.idle.len() + inner.active.len() <= self.max_idle {
                                inner.idle.push(conn);
                            }
                            return Ok(Ok(r));
                        }
                        Err(b) => {
                            // The connection is closing
                            self.inner
                                .lock()
                                .trace_expect("Failed to lock mutex")
                                .active
                                .remove(&conn.local_addr);
                            bundle = b
                        }
                    }
                }
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
            loop {
                let conn_tx = self
                    .inner
                    .lock()
                    .trace_expect("Failed to lock mutex")
                    .active
                    .values()
                    .choose(&mut rand::rng())
                    .cloned();

                let Some(conn_tx) = conn_tx else {
                    break;
                };

                let (tx, rx) = tokio::sync::oneshot::channel();
                if let Err(e) = conn_tx.send((bundle, tx)).await {
                    bundle = e.0.0;
                } else {
                    match rx.await.trace_expect("Sender dropped!") {
                        Ok(r) => {
                            return Ok(Ok(r));
                        }
                        Err(b) => bundle = b,
                    }
                }
            }
        }
    }
}

pub struct ConnectionRegistry {
    pools: Mutex<HashMap<SocketAddr, Arc<connection::ConnectionPool>>>,
    peers: Mutex<HashMap<SocketAddr, Eid>>,
    sink: Arc<dyn hardy_bpa::cla::Sink>,
    max_idle: usize,
}

impl ConnectionRegistry {
    pub fn new(sink: Arc<dyn hardy_bpa::cla::Sink>, max_idle: usize) -> Self {
        Self {
            pools: Mutex::new(HashMap::new()),
            peers: Mutex::new(HashMap::new()),
            sink,
            max_idle,
        }
    }

    pub async fn shutdown(&self) {
        // Unregister peers
        let peers = std::mem::take(&mut *self.peers.lock().trace_expect("Failed to lock mutex"));

        for (addr, eid) in peers {
            if let Err(e) = self
                .sink
                .remove_peer(&eid, &hardy_bpa::cla::ClaAddress::TcpClv4Address(addr))
                .await
            {
                error!("Failed to unregister peer: {e:?}");
            }
        }

        // This will close the tx end of the channels, which should cause the session::run tasks to exit
        self.pools
            .lock()
            .trace_expect("Failed to lock mutex")
            .clear();
    }

    pub async fn register_session(
        &self,
        conn: Connection,
        remote_addr: SocketAddr,
        eid: Option<Eid>,
    ) {
        match self
            .pools
            .lock()
            .trace_expect("Failed to lock mutex")
            .entry(remote_addr)
        {
            std::collections::hash_map::Entry::Occupied(mut e) => {
                e.get_mut().add(conn);
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(Arc::new(connection::ConnectionPool::new(
                    conn,
                    self.max_idle,
                )));
            }
        }

        if let Some(eid) = eid
            && self
                .peers
                .lock()
                .trace_expect("Failed to lock mutex")
                .insert(remote_addr, eid.clone())
                .is_none()
            && let Err(e) = self
                .sink
                .add_peer(eid, hardy_bpa::cla::ClaAddress::TcpClv4Address(remote_addr))
                .await
        {
            error!("add_peer failed: {e:?}");
        }
    }

    pub async fn unregister_session(&self, local_addr: &SocketAddr, remote_addr: &SocketAddr) {
        {
            let mut pools = self.pools.lock().trace_expect("Failed to lock mutex");
            if let Some(e) = pools.get_mut(remote_addr)
                && e.remove(local_addr)
            {
                pools.remove(remote_addr);
            }
        }

        let peer = self
            .peers
            .lock()
            .trace_expect("Failed to lock mutex")
            .remove_entry(remote_addr);

        if let Some((addr, eid)) = peer
            && let Err(e) = self
                .sink
                .remove_peer(&eid, &hardy_bpa::cla::ClaAddress::TcpClv4Address(addr))
                .await
        {
            error!("Failed to unregister peer: {e:?}");
        }
    }

    pub async fn forward(
        &self,
        remote_addr: &SocketAddr,
        mut bundle: hardy_bpa::Bytes,
    ) -> Result<hardy_bpa::cla::Result<hardy_bpa::cla::ForwardBundleResult>, hardy_bpa::Bytes> {
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
