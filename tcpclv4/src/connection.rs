use super::*;
use rand::seq::IteratorRandom;
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    slice,
    sync::{Arc, Mutex},
};

// What a forward should do when every pooled session is busy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnBusy {
    // Signal the caller to dial a new connection while the pool has capacity
    Dial,
    // Queue on a busy session — the fallback for peers that cannot accept
    // another connection
    Queue,
}

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
            && !self
                .sink
                .add_peer(self.remote_addr.clone(), slice::from_ref(&node_id))
                .await
                .unwrap_or_else(|e| {
                    warn!("add_peer failed: {e:?}");
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
        let remove_addr = {
            let mut inner = self.inner.lock().trace_expect("Failed to lock mutex");
            inner.active.remove(local_addr);
            let before = inner.idle.len();
            inner.idle.retain(|c| &c.local_addr != local_addr);
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

        if remove_addr {
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
        on_busy: OnBusy,
    ) -> Result<hardy_bpa::cla::ForwardBundleResult, hardy_bpa::Bytes> {
        // We repeatedly search as this function is async, so changes can happen
        // while running. Cap the retries so a peer whose sessions repeatedly
        // accept-then-fail (while the pool stays above max_idle) can't wedge the
        // forward indefinitely.
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
                        if inner.idle.len() + inner.active.len() < self.max_idle {
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

            // No idle sessions: prefer dialing a new connection over queueing
            // behind a busy session while the pool has capacity. Concurrent
            // forwards may each signal a dial, briefly overshooting the bound;
            // excess connections are shed after use by the idle-return check
            // above.
            if on_busy == OnBusy::Dial
                && (self.max_idle == 0 || {
                    let inner = self.inner.lock().trace_expect("Failed to lock mutex");
                    inner.active.len() + inner.idle.len()
                } < self.max_idle)
            {
                return Err(bundle);
            }

            // At capacity (or dialing is not an option): queue on a random
            // active connection
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

            // Nothing could send. Retry — bounded, and yielding so the tasks
            // that manage the pool get a chance to run.
            retries += 1;
            if retries >= MAX_RETRIES {
                return Err(bundle);
            }
            tokio::task::yield_now().await;
        }
    }
}

pub struct ConnectionRegistry {
    pools: Mutex<HashMap<SocketAddr, Arc<connection::ConnectionPool>>>,
    // One dial at a time per remote address: concurrent forwards that find
    // the pool busy coalesce on this lock instead of racing parallel dials
    // past the capacity bound. Entries are never removed; the map is bounded
    // by the deployment's peer-address cardinality.
    dial_locks: Mutex<HashMap<SocketAddr, Arc<tokio::sync::Mutex<()>>>>,
    max_idle: usize,
}

impl ConnectionRegistry {
    pub fn new(max_idle: usize) -> Self {
        Self {
            pools: Mutex::new(HashMap::new()),
            dial_locks: Mutex::new(HashMap::new()),
            max_idle,
        }
    }

    // Whether any session (idle or active) currently exists for `remote_addr`.
    pub fn has_sessions(&self, remote_addr: &SocketAddr) -> bool {
        self.pools
            .lock()
            .trace_expect("Failed to lock mutex")
            .get(remote_addr)
            .is_some_and(|pool| {
                let inner = pool.inner.lock().trace_expect("Failed to lock mutex");
                !inner.active.is_empty() || !inner.idle.is_empty()
            })
    }

    // The per-address dial lock. Callers re-check the pool after acquisition:
    // the previous holder's session may already be registered.
    pub fn dial_lock(&self, remote_addr: SocketAddr) -> Arc<tokio::sync::Mutex<()>> {
        self.dial_locks
            .lock()
            .trace_expect("Failed to lock mutex")
            .entry(remote_addr)
            .or_default()
            .clone()
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    pub fn shutdown(&self) {
        let mut pools = self.pools.lock().trace_expect("Failed to lock mutex");

        // Count remaining idle connections before clearing
        let idle: usize = pools.values().map(|pool| pool.idle_count()).sum();
        if idle > 0 {
            metrics::gauge!("tcpclv4.pool.idle").decrement(idle as f64);
        }

        // Closing tx channels causes session::run tasks to exit
        pools.clear();
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

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
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
            let mut pools = self.pools.lock().trace_expect("Failed to lock mutex");
            if let Some(current) = pools.get(remote_addr)
                && Arc::ptr_eq(current, &pool)
            {
                pools.remove(remote_addr);
            }
        }
    }

    // Forward a bundle over a pooled session for `remote_addr`.
    //
    // With `OnBusy::Dial`, Err(bundle) signals the caller to establish a new
    // connection (the pool has capacity and no session is free). With
    // `OnBusy::Queue`, busy sessions are queued on instead — the fallback for
    // peers that hold a session open but cannot accept new connections.
    #[cfg_attr(feature = "instrument", instrument(skip(self, bundle)))]
    pub async fn forward(
        &self,
        remote_addr: &SocketAddr,
        mut bundle: hardy_bpa::Bytes,
        on_busy: OnBusy,
    ) -> Result<hardy_bpa::cla::ForwardBundleResult, hardy_bpa::Bytes> {
        let pool = self
            .pools
            .lock()
            .trace_expect("Failed to lock mutex")
            .get(remote_addr)
            .cloned();

        if let Some(pool) = pool {
            match pool.try_send(bundle, on_busy).await {
                Ok(r) => return Ok(r),
                Err(b) => {
                    bundle = b;
                }
            }
        }
        Err(bundle)
    }
}

#[cfg(test)]
mod tests {
    use hardy_bpa::async_trait;

    use super::*;

    type ConnectionRx = tokio::sync::mpsc::Receiver<(
        hardy_bpa::Bytes,
        tokio::sync::oneshot::Sender<hardy_bpa::cla::ForwardBundleResult>,
    )>;

    struct MockSink;

    #[async_trait]
    impl hardy_bpa::cla::Sink for MockSink {
        async fn unregister(&self) {}

        async fn dispatch(
            &self,
            _bundle: hardy_bpa::Bytes,
            _peer_node: Option<&NodeId>,
            _peer_addr: Option<&hardy_bpa::cla::ClaAddress>,
        ) -> hardy_bpa::cla::Result<()> {
            Ok(())
        }

        async fn add_peer(
            &self,
            _cla_addr: hardy_bpa::cla::ClaAddress,
            _node_ids: &[NodeId],
        ) -> hardy_bpa::cla::Result<bool> {
            Ok(true)
        }

        async fn remove_peer(
            &self,
            _cla_addr: &hardy_bpa::cla::ClaAddress,
        ) -> hardy_bpa::cla::Result<bool> {
            Ok(true)
        }
    }

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn conn(port: u16) -> (Connection, ConnectionRx) {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        (
            Connection {
                tx,
                local_addr: addr(port),
            },
            rx,
        )
    }

    // A stand-in session that accepts every bundle and reports it sent
    fn serve_sent(mut rx: ConnectionRx) {
        tokio::spawn(async move {
            while let Some((_bundle, result)) = rx.recv().await {
                _ = result.send(hardy_bpa::cla::ForwardBundleResult::Sent);
            }
        });
    }

    // Move the pool's sole idle connection into the active (busy) set
    fn make_busy(pool: &ConnectionPool) {
        let mut inner = pool.inner.lock().unwrap();
        let conn = inner.idle.pop().unwrap();
        inner.active.insert(conn.local_addr, conn.tx);
    }

    #[tokio::test]
    async fn dials_when_under_capacity_and_all_sessions_busy() {
        let (conn, _rx) = conn(1);
        let pool = ConnectionPool::new(conn, Arc::new(MockSink), addr(4556), 6);
        make_busy(&pool);

        // Must signal a dial without blocking on the busy session
        let r = tokio::time::timeout(
            tokio::time::Duration::from_secs(1),
            pool.try_send(hardy_bpa::Bytes::from_static(b"bundle"), OnBusy::Dial),
        )
        .await
        .expect("try_send queued on a busy session instead of signalling a dial");
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn queues_on_busy_session_at_capacity() {
        let (conn1, rx1) = conn(1);
        let (conn2, rx2) = conn(2);
        serve_sent(rx1);
        serve_sent(rx2);

        // A max_idle of 1 with two busy connections puts the pool over
        // capacity, so the forward queues rather than dialling
        let pool = ConnectionPool::new(conn1, Arc::new(MockSink), addr(4556), 1);
        make_busy(&pool);
        pool.inner
            .lock()
            .unwrap()
            .active
            .insert(conn2.local_addr, conn2.tx);

        let r = pool
            .try_send(hardy_bpa::Bytes::from_static(b"bundle"), OnBusy::Dial)
            .await;
        assert!(matches!(r, Ok(hardy_bpa::cla::ForwardBundleResult::Sent)));
    }

    #[tokio::test]
    async fn no_dial_forward_queues_on_busy_session() {
        let (conn, rx) = conn(1);
        serve_sent(rx);

        let pool = ConnectionPool::new(conn, Arc::new(MockSink), addr(4556), 6);
        make_busy(&pool);

        // With dialling ruled out, the forward queues despite spare capacity
        let r = pool
            .try_send(hardy_bpa::Bytes::from_static(b"bundle"), OnBusy::Queue)
            .await;
        assert!(matches!(r, Ok(hardy_bpa::cla::ForwardBundleResult::Sent)));
    }

    #[tokio::test]
    async fn idle_session_is_used_and_returned() {
        let (conn, rx) = conn(1);
        serve_sent(rx);

        let pool = ConnectionPool::new(conn, Arc::new(MockSink), addr(4556), 6);
        let r = pool
            .try_send(hardy_bpa::Bytes::from_static(b"bundle"), OnBusy::Dial)
            .await;
        assert!(matches!(r, Ok(hardy_bpa::cla::ForwardBundleResult::Sent)));

        let inner = pool.inner.lock().unwrap();
        assert_eq!(inner.idle.len(), 1);
        assert!(inner.active.is_empty());
    }
}
