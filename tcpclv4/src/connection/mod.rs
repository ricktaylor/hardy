// Connection lifecycle, grouped in one place: the shared bring-up context
// (`context`), the active dialer (`connect`), the passive listener
// (`listen`), and — in this file — the pooled registry of established
// sessions.

pub mod connect;
pub mod context;
pub mod listen;

use super::*;
use rand::seq::IteratorRandom;
use std::{
    collections::HashMap,
    net::SocketAddr,
    slice,
    sync::{Arc, Mutex},
};

// How a session came to be: dialled by this entity or accepted from the
// peer. A pool's address can be dialled only once a dialled session has
// reached it; the address of an accepted session is the peer's source
// address, normally an ephemeral port that accepts nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    Dialled,
    Accepted,
}

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
    tokio::sync::oneshot::Sender<hardy_bpa::cla::TransferOutcome>,
)>;

pub struct Connection {
    pub tx: ConnectionTx,
    pub local_addr: SocketAddr,
}

struct ConnectionPoolInner {
    active: HashMap<SocketAddr, ConnectionTx>,
    idle: Vec<Connection>,
    // Whether the pool's address accepts connections: true once a dialled
    // session has joined the pool. A pool of accepted sessions never dials.
    dialable: bool,
    // The node the pool's adjacency names: the first node ID a session in
    // the pool announced. A later session naming another node joins the
    // pool without an adjacency of its own (see Peer identity in the
    // design doc).
    node: Option<NodeId>,
    // Whether the BPA holds the pool's adjacency. Changed only under the
    // registry's membership lock; a pool of accepted sessions withholds its
    // adjacency while a dialable pool holds one for the same node.
    advertised: bool,
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
        origin: Origin,
        sink: Arc<dyn hardy_bpa::cla::Sink>,
        remote_addr: SocketAddr,
        max_idle: usize,
    ) -> Self {
        metrics::gauge!("tcpclv4.pool.idle").increment(1.0);
        Self {
            inner: Mutex::new(ConnectionPoolInner {
                active: HashMap::new(),
                idle: vec![conn],
                dialable: origin == Origin::Dialled,
                node: None,
                advertised: false,
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

    fn add(&self, conn: Connection, origin: Origin) {
        let mut inner = self.inner.lock().trace_expect("Failed to lock mutex");
        inner.idle.push(conn);
        inner.dialable |= origin == Origin::Dialled;
        metrics::gauge!("tcpclv4.pool.idle").increment(1.0);
    }

    // The node a session announced becomes the pool's, unless the pool
    // already names one. Returns the pool's node.
    fn name_node(&self, node_id: Option<NodeId>) -> Option<NodeId> {
        let mut inner = self.inner.lock().trace_expect("Failed to lock mutex");
        match (&inner.node, node_id) {
            (None, node_id) => inner.node = node_id,
            (Some(node), Some(node_id)) if *node != node_id => {
                debug!(
                    "A session at {} names {node_id}, but its pool's adjacency names {node}",
                    self.remote_addr
                );
            }
            _ => {}
        }
        inner.node.clone()
    }

    fn is_advertised(&self) -> bool {
        self.inner
            .lock()
            .trace_expect("Failed to lock mutex")
            .advertised
    }

    // The pool's node, whether its address can be dialled, and whether
    // the BPA holds its adjacency.
    fn adjacency(&self) -> (Option<NodeId>, bool, bool) {
        let inner = self.inner.lock().trace_expect("Failed to lock mutex");
        (inner.node.clone(), inner.dialable, inner.advertised)
    }

    // Report the pool's adjacency for `node` to the BPA. Called under the
    // registry's membership lock.
    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    async fn advertise(&self, node: &NodeId) {
        let added = self
            .sink
            .add_peer(self.remote_addr.clone(), slice::from_ref(node))
            .await
            .unwrap_or_else(|e| {
                warn!("add_peer failed: {e:?}");
                false
            });
        self.inner
            .lock()
            .trace_expect("Failed to lock mutex")
            .advertised = added;
    }

    // Withdraw the pool's adjacency from the BPA. Called under the
    // registry's membership lock.
    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    async fn withdraw(&self) {
        self.inner
            .lock()
            .trace_expect("Failed to lock mutex")
            .advertised = false;
        _ = self.sink.remove_peer(&self.remote_addr).await;
    }

    // Remove a session from the pool; returns whether the pool is now empty.
    fn remove(&self, local_addr: &SocketAddr) -> bool {
        let mut inner = self.inner.lock().trace_expect("Failed to lock mutex");
        inner.active.remove(local_addr);
        let before = inner.idle.len();
        inner.idle.retain(|c| &c.local_addr != local_addr);
        let removed = before - inner.idle.len();
        if removed > 0 {
            metrics::gauge!("tcpclv4.pool.idle").decrement(removed as f64);
        }
        inner.active.is_empty() && inner.idle.is_empty()
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, bundle)))]
    async fn try_send(
        &self,
        bundle: hardy_bpa::Bytes,
        on_busy: OnBusy,
    ) -> Result<hardy_bpa::cla::TransferOutcome, hardy_bpa::Bytes> {
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
            // behind a busy session while the pool has capacity and its
            // address can be dialled. Concurrent forwards may each signal a
            // dial, briefly overshooting the bound; excess connections are
            // shed after use by the idle-return check above.
            if on_busy == OnBusy::Dial && {
                let inner = self.inner.lock().trace_expect("Failed to lock mutex");
                inner.dialable
                    && (self.max_idle == 0 || inner.active.len() + inner.idle.len() < self.max_idle)
            } {
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
    // Serializes membership: session registration and unregistration take
    // it first and hold it across the pool decision, the change to `pools`
    // and the BPA's add_peer/remove_peer, so a session registering at an
    // address whose pool is being torn down waits for the teardown and
    // starts a fresh pool, and an adjacency is never withdrawn while it is
    // still being added. One lock for the whole registry: membership
    // changes are rare (a session coming or going), and forwarding never
    // takes it.
    membership: tokio::sync::Mutex<()>,
    // Woken whenever a pool leaves the registry, for waiters on an
    // address losing its last session.
    pool_closed: tokio::sync::Notify,
    // One dial at a time per remote address: concurrent forwards that find
    // the pool busy coalesce on this lock instead of racing parallel dials
    // past the capacity bound. An entry is created when the first dialled
    // session registers and is never removed, so the map is bounded by the
    // addresses this entity has dialled, and a forward never dials an
    // address without one: a pool of accepted sessions, live or gone.
    dial_locks: Mutex<HashMap<SocketAddr, Arc<tokio::sync::Mutex<()>>>>,
    // Bounds transfers accepted but not yet resolved with an outcome, per
    // peer: a forward holds a permit from acceptance until its outcome is
    // reported, and an exhausted semaphore withholds further forward
    // verdicts — the flow control back to the BPA's per-peer egress poller.
    // Per-peer scoping keeps one unreachable peer's stalled dials from
    // starving admission for healthy peers. Entries are evicted with the
    // peer's session pool; the `Arc` held by any in-flight transfer keeps
    // its semaphore (and permit accounting) alive across eviction.
    transfer_permits: Mutex<HashMap<SocketAddr, Arc<tokio::sync::Semaphore>>>,
    max_idle: usize,
    max_outstanding_transfers: core::num::NonZeroUsize,
}

impl ConnectionRegistry {
    pub fn new(max_idle: usize, max_outstanding_transfers: core::num::NonZeroUsize) -> Self {
        Self {
            pools: Mutex::new(HashMap::new()),
            membership: tokio::sync::Mutex::new(()),
            pool_closed: tokio::sync::Notify::new(),
            dial_locks: Mutex::new(HashMap::new()),
            transfer_permits: Mutex::new(HashMap::new()),
            max_idle,
            max_outstanding_transfers,
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

    // Waits until no session exists for `remote_addr`.
    pub async fn sessions_closed(&self, remote_addr: &SocketAddr) {
        loop {
            // Enabled before the check, so a pool closing in between still
            // wakes this waiter
            let closed = self.pool_closed.notified();
            let mut closed = core::pin::pin!(closed);
            closed.as_mut().enable();
            if !self.has_sessions(remote_addr) {
                return;
            }
            closed.await;
        }
    }

    // The per-address dial lock, `None` for an address no dialled session
    // has reached. Callers re-check the pool after acquisition: the previous
    // holder's session may already be registered.
    pub fn dial_lock(&self, remote_addr: &SocketAddr) -> Option<Arc<tokio::sync::Mutex<()>>> {
        self.dial_locks
            .lock()
            .trace_expect("Failed to lock mutex")
            .get(remote_addr)
            .cloned()
    }

    // The per-address transfer-permit semaphore, created on first use.
    pub fn transfer_permits(&self, remote_addr: SocketAddr) -> Arc<tokio::sync::Semaphore> {
        self.transfer_permits
            .lock()
            .trace_expect("Failed to lock mutex")
            .entry(remote_addr)
            .or_insert_with(|| {
                Arc::new(tokio::sync::Semaphore::new(
                    self.max_outstanding_transfers.get(),
                ))
            })
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
        self.pool_closed.notify_waiters();

        self.transfer_permits
            .lock()
            .trace_expect("Failed to lock mutex")
            .clear();
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, sink, conn)))]
    pub async fn register_session(
        &self,
        sink: Arc<dyn hardy_bpa::cla::Sink>,
        conn: Connection,
        remote_addr: SocketAddr,
        origin: Origin,
        node_id: Option<NodeId>,
    ) {
        let _membership = self.membership.lock().await;

        if origin == Origin::Dialled {
            self.dial_locks
                .lock()
                .trace_expect("Failed to lock mutex")
                .entry(remote_addr)
                .or_default();
        }

        let pool = match self
            .pools
            .lock()
            .trace_expect("Failed to lock mutex")
            .entry(remote_addr)
        {
            std::collections::hash_map::Entry::Occupied(mut e) => {
                let pool = e.get_mut();
                pool.add(conn, origin);
                pool.clone()
            }
            std::collections::hash_map::Entry::Vacant(e) => e
                .insert(Arc::new(connection::ConnectionPool::new(
                    conn,
                    origin,
                    sink,
                    remote_addr,
                    self.max_idle,
                )))
                .clone(),
        };

        if let Some(node) = pool.name_node(node_id) {
            self.reconcile(&node).await;
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    pub async fn unregister_session(&self, local_addr: &SocketAddr, remote_addr: &SocketAddr) {
        let _membership = self.membership.lock().await;

        let pool = {
            let mut pools = self.pools.lock().trace_expect("Failed to lock mutex");
            let Some(pool) = pools.get(remote_addr).cloned() else {
                return;
            };
            if !pool.remove(local_addr) {
                return;
            }
            pools.remove(remote_addr);
            pool
        };
        self.pool_closed.notify_waiters();

        // The peer's session pool is gone: evict its permit semaphore so
        // the map tracks peer churn instead of growing monotonically.
        // In-flight transfers keep the semaphore alive through their own
        // `Arc`s, and their bundles are swept back to Waiting by the peer
        // removal anyway.
        self.transfer_permits
            .lock()
            .trace_expect("Failed to lock mutex")
            .remove(remote_addr);

        // Reconcile the node's other pools before withdrawing this one, so
        // a withheld pool of accepted sessions takes over the node without
        // a gap when its last dialable pool goes
        let (node, _, advertised) = pool.adjacency();
        if let Some(node) = node {
            self.reconcile(&node).await;
        }
        if advertised {
            pool.withdraw().await;
        }
    }

    // Bring the BPA's adjacencies for `node` in line with its pools: every
    // dialable pool naming the node is advertised, and a pool of accepted
    // sessions is advertised only while no dialable pool's adjacency is
    // held. An accepted session's pool is keyed by the peer's source port:
    // it cannot grow, so it carries one transfer at a time, while a
    // dialled pool grows to `max_idle` sessions; and the BPA's ECMP would
    // pick between them per flow. The withheld pool's sessions still
    // deliver what the peer sends, and still carry transfers already
    // handed to them. Called under the membership lock.
    async fn reconcile(&self, node: &NodeId) {
        let pools: Vec<_> = self
            .pools
            .lock()
            .trace_expect("Failed to lock mutex")
            .values()
            .filter_map(|pool| {
                let (pool_node, dialable, advertised) = pool.adjacency();
                (pool_node.as_ref() == Some(node)).then(|| (pool.clone(), dialable, advertised))
            })
            .collect();

        // The new adjacency goes in before the old ones come out, so the
        // node stays routable throughout
        for (pool, _, _) in pools
            .iter()
            .filter(|(_, dialable, advertised)| *dialable && !*advertised)
        {
            pool.advertise(node).await;
        }
        let preferred = pools
            .iter()
            .any(|(pool, dialable, _)| *dialable && pool.is_advertised());

        for (pool, _, advertised) in pools.iter().filter(|(_, dialable, _)| !*dialable) {
            match (preferred, *advertised) {
                (false, false) => pool.advertise(node).await,
                (true, true) => {
                    debug!(
                        "Withholding {} for {node}: a dialled pool holds the adjacency",
                        pool.remote_addr
                    );
                    pool.withdraw().await
                }
                _ => {}
            }
        }
    }

    // Forward a bundle over a pooled session for `remote_addr`.
    //
    // With `OnBusy::Dial`, Err(bundle) signals the caller to establish a new
    // connection (the pool's address can be dialled, the pool has capacity
    // and no session is free); a busy pool of accepted sessions queues. With
    // `OnBusy::Queue`, busy sessions are queued on instead — the fallback for
    // peers that hold a session open but cannot accept new connections.
    #[cfg_attr(feature = "instrument", instrument(skip(self, bundle)))]
    pub async fn forward(
        &self,
        remote_addr: &SocketAddr,
        mut bundle: hardy_bpa::Bytes,
        on_busy: OnBusy,
    ) -> Result<hardy_bpa::cla::TransferOutcome, hardy_bpa::Bytes> {
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
    use std::pin::pin;

    use futures::{future::maybe_done, poll};
    use hardy_bpa::async_trait;

    use super::*;

    type ConnectionRx = tokio::sync::mpsc::Receiver<(
        hardy_bpa::Bytes,
        tokio::sync::oneshot::Sender<hardy_bpa::cla::TransferOutcome>,
    )>;

    struct MockSink;

    #[async_trait]
    impl hardy_bpa::cla::Sink for MockSink {
        async fn unregister(&self) {}

        async fn dispatch(
            &self,
            _peer_node: Option<&NodeId>,
            _peer_addr: Option<&hardy_bpa::cla::ClaAddress>,
            _stream: &mut dyn hardy_bpa::stream::Receiver<hardy_bpa::cla::Segment>,
        ) -> hardy_bpa::cla::Result<hardy_bpa::cla::Acceptance> {
            Ok(hardy_bpa::cla::Acceptance::Accepted)
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

        async fn transfer_outcome(
            &self,
            _bundle_id: &hardy_bpv7::bundle::Id,
            _outcome: hardy_bpa::cla::TransferOutcome,
        ) -> hardy_bpa::cla::Result<()> {
            Ok(())
        }
    }

    // A sink that keeps the BPA's adjacency claims: one adjacency per
    // address, so add_peer on a claimed address is refused (as
    // `ClaRegistry::add_peer` does), and remove_peer releases the claim.
    // Arming `hold_remove` parks the next remove_peer before it releases
    // the claim, until `release_remove` fires: a BPA that has not yet
    // processed the removal, as over gRPC.
    #[derive(Default)]
    struct ClaimingSink {
        claims: Mutex<HashMap<hardy_bpa::cla::ClaAddress, Vec<NodeId>>>,
        hold_remove: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
        calls: Mutex<Vec<Call>>,
    }

    // A sink call that changed the claims
    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        Add(hardy_bpa::cla::ClaAddress),
        Remove(hardy_bpa::cla::ClaAddress),
    }

    impl ClaimingSink {
        fn hold_next_remove(&self) -> tokio::sync::oneshot::Sender<()> {
            let (release_remove, held) = tokio::sync::oneshot::channel();
            *self.hold_remove.lock().unwrap() = Some(held);
            release_remove
        }

        fn claims(&self) -> HashMap<hardy_bpa::cla::ClaAddress, Vec<NodeId>> {
            self.claims.lock().unwrap().clone()
        }

        fn take_calls(&self) -> Vec<Call> {
            core::mem::take(&mut *self.calls.lock().unwrap())
        }
    }

    #[async_trait]
    impl hardy_bpa::cla::Sink for ClaimingSink {
        async fn unregister(&self) {}

        async fn dispatch(
            &self,
            _peer_node: Option<&NodeId>,
            _peer_addr: Option<&hardy_bpa::cla::ClaAddress>,
            _stream: &mut dyn hardy_bpa::stream::Receiver<hardy_bpa::cla::Segment>,
        ) -> hardy_bpa::cla::Result<hardy_bpa::cla::Acceptance> {
            Ok(hardy_bpa::cla::Acceptance::Accepted)
        }

        async fn add_peer(
            &self,
            cla_addr: hardy_bpa::cla::ClaAddress,
            node_ids: &[NodeId],
        ) -> hardy_bpa::cla::Result<bool> {
            Ok(match self.claims.lock().unwrap().entry(cla_addr.clone()) {
                std::collections::hash_map::Entry::Occupied(_) => false,
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(node_ids.to_vec());
                    self.calls.lock().unwrap().push(Call::Add(cla_addr));
                    true
                }
            })
        }

        async fn remove_peer(
            &self,
            cla_addr: &hardy_bpa::cla::ClaAddress,
        ) -> hardy_bpa::cla::Result<bool> {
            let held = self.hold_remove.lock().unwrap().take();
            if let Some(held) = held {
                _ = held.await;
            }
            let removed = self.claims.lock().unwrap().remove(cla_addr).is_some();
            if removed {
                self.calls
                    .lock()
                    .unwrap()
                    .push(Call::Remove(cla_addr.clone()));
            }
            Ok(removed)
        }

        async fn transfer_outcome(
            &self,
            _bundle_id: &hardy_bpv7::bundle::Id,
            _outcome: hardy_bpa::cla::TransferOutcome,
        ) -> hardy_bpa::cla::Result<()> {
            Ok(())
        }
    }

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn node(n: u32) -> NodeId {
        NodeId::Ipn(hardy_bpv7::eid::IpnNodeId {
            allocator_id: 0,
            node_number: n,
        })
    }

    fn tcp(port: u16) -> hardy_bpa::cla::ClaAddress {
        hardy_bpa::cla::ClaAddress::Tcp(addr(port))
    }

    fn registry() -> ConnectionRegistry {
        ConnectionRegistry::new(6, core::num::NonZeroUsize::new(16).unwrap())
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

    // A stand-in session that accepts every bundle and reports it completed
    fn serve_completed(mut rx: ConnectionRx) {
        tokio::spawn(async move {
            while let Some((_bundle, result)) = rx.recv().await {
                _ = result.send(hardy_bpa::cla::TransferOutcome::Completed);
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
        let pool = ConnectionPool::new(conn, Origin::Dialled, Arc::new(MockSink), addr(4556), 6);
        make_busy(&pool);

        // Must signal a dial without blocking on the busy session
        let r = tokio::time::timeout(
            tokio::time::Duration::from_secs(1),
            pool.try_send(hardy_bpa::Bytes::from_static(b"bundle"), OnBusy::Dial),
        )
        .await
        .expect("try_send queued on a busy session instead of signalling a dial");
        assert_eq!(r, Err(hardy_bpa::Bytes::from_static(b"bundle")));
    }

    #[tokio::test]
    async fn queues_on_busy_session_at_capacity() {
        let (conn1, rx1) = conn(1);
        let (conn2, rx2) = conn(2);
        serve_completed(rx1);
        serve_completed(rx2);

        // A max_idle of 1 with two busy connections puts the pool over
        // capacity, so the forward queues rather than dialling
        let pool = ConnectionPool::new(conn1, Origin::Dialled, Arc::new(MockSink), addr(4556), 1);
        make_busy(&pool);
        pool.inner
            .lock()
            .unwrap()
            .active
            .insert(conn2.local_addr, conn2.tx);

        let r = pool
            .try_send(hardy_bpa::Bytes::from_static(b"bundle"), OnBusy::Dial)
            .await;
        assert!(matches!(r, Ok(hardy_bpa::cla::TransferOutcome::Completed)));
    }

    #[tokio::test]
    async fn no_dial_forward_queues_on_busy_session() {
        let (conn, rx) = conn(1);
        serve_completed(rx);

        let pool = ConnectionPool::new(conn, Origin::Dialled, Arc::new(MockSink), addr(4556), 6);
        make_busy(&pool);

        // With dialling ruled out, the forward queues despite spare capacity
        let r = pool
            .try_send(hardy_bpa::Bytes::from_static(b"bundle"), OnBusy::Queue)
            .await;
        assert!(matches!(r, Ok(hardy_bpa::cla::TransferOutcome::Completed)));
    }

    // A pool of accepted sessions is keyed by the peer's source address,
    // which accepts no connections: busy, it queues on a session rather
    // than signalling a dial, whatever its spare capacity.
    #[tokio::test]
    async fn busy_pool_of_accepted_sessions_queues_instead_of_dialling() {
        let (conn, rx) = conn(1);
        serve_completed(rx);

        let pool = ConnectionPool::new(conn, Origin::Accepted, Arc::new(MockSink), addr(50000), 6);
        make_busy(&pool);

        let r = pool
            .try_send(hardy_bpa::Bytes::from_static(b"bundle"), OnBusy::Dial)
            .await;
        assert!(matches!(r, Ok(hardy_bpa::cla::TransferOutcome::Completed)));
    }

    // A dialled session joining a pool of accepted sessions at the same
    // address proves the address accepts connections, so the pool grows by
    // dialling from then on.
    #[tokio::test]
    async fn dialled_session_makes_its_pool_dialable() {
        let (conn1, _rx1) = conn(1);
        let (conn2, _rx2) = conn(2);
        let pool = ConnectionPool::new(conn1, Origin::Accepted, Arc::new(MockSink), addr(4556), 6);
        pool.add(conn2, Origin::Dialled);
        make_busy(&pool);
        make_busy(&pool);

        // the timeout only bounds a regression: a dial signal returns at once
        let r = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            pool.try_send(hardy_bpa::Bytes::from_static(b"bundle"), OnBusy::Dial),
        )
        .await
        .expect("try_send queued on a busy session instead of signalling a dial");
        assert_eq!(r, Err(hardy_bpa::Bytes::from_static(b"bundle")));
    }

    // Only an address a dialled session has reached gets a dial lock, so a
    // forward can never dial a peer's source address, even once the pool of
    // its accepted sessions has gone, and the lock map stays bounded by the
    // dialled addresses.
    #[tokio::test]
    async fn only_dialled_addresses_get_a_dial_lock() {
        let registry = ConnectionRegistry::new(6, core::num::NonZeroUsize::new(16).unwrap());
        let (accepted, _rx1) = conn(1);
        let (dialled, _rx2) = conn(2);

        registry
            .register_session(
                Arc::new(MockSink),
                accepted,
                addr(50000),
                Origin::Accepted,
                None,
            )
            .await;
        registry
            .register_session(
                Arc::new(MockSink),
                dialled,
                addr(4556),
                Origin::Dialled,
                None,
            )
            .await;
        registry.unregister_session(&addr(1), &addr(50000)).await;
        registry.unregister_session(&addr(2), &addr(4556)).await;

        assert!(registry.dial_lock(&addr(50000)).is_none());
        assert!(registry.dial_lock(&addr(4556)).is_some());
    }

    // A session that registers at an address while the address's last
    // session is tearing its pool down, the BPA's remove_peer still in
    // flight, waits for the teardown: it starts a fresh pool, which then
    // gets the adjacency. Joining the departing pool instead would strand
    // the session in a pool about to leave the registry, its add_peer
    // refused against the claim still held, or accepted after the removal
    // with nothing left to withdraw it.
    #[tokio::test]
    async fn registration_waits_for_a_teardown_in_flight_at_its_address() {
        let sink = Arc::new(ClaimingSink::default());
        let registry = registry();
        let (first, _rx1) = conn(1);
        let (second, _rx2) = conn(2);

        registry
            .register_session(
                sink.clone(),
                first,
                addr(50000),
                Origin::Accepted,
                Some(node(2)),
            )
            .await;
        assert_eq!(sink.claims(), HashMap::from([(tcp(50000), vec![node(2)])]));

        // The last session ends; teardown parks inside the BPA's remove_peer
        let release_remove = sink.hold_next_remove();
        let (local, remote) = (addr(1), addr(50000));
        let mut teardown = pin!(registry.unregister_session(&local, &remote));
        assert!(poll!(teardown.as_mut()).is_pending());

        // A new session from the same address registers meanwhile
        let mut registration = pin!(maybe_done(registry.register_session(
            sink.clone(),
            second,
            addr(50000),
            Origin::Accepted,
            Some(node(2)),
        )));
        _ = poll!(registration.as_mut());

        _ = release_remove.send(());
        teardown.await;
        registration.await;

        assert!(registry.has_sessions(&addr(50000)));
        assert_eq!(sink.claims(), HashMap::from([(tcp(50000), vec![node(2)])]));

        // And the adjacency leaves with the new session
        registry.unregister_session(&addr(2), &addr(50000)).await;
        assert!(sink.claims().is_empty());
    }

    // Register a session for `node` at `port`, its local address `local`
    async fn register(
        registry: &ConnectionRegistry,
        sink: &Arc<ClaimingSink>,
        local: u16,
        port: u16,
        origin: Origin,
        node_id: NodeId,
    ) -> ConnectionRx {
        let (conn, rx) = conn(local);
        registry
            .register_session(sink.clone(), conn, addr(port), origin, Some(node_id))
            .await;
        rx
    }

    // A pool of accepted sessions withholds its adjacency for a node while
    // a dialled pool holds one, so the BPA forwards to the node over the
    // pool that can grow, and a forged session claiming the node draws no
    // traffic. A pool for another node is unaffected.
    #[tokio::test]
    async fn accepted_pool_withholds_its_adjacency_while_a_dialled_pool_holds_one() {
        let sink = Arc::new(ClaimingSink::default());
        let registry = registry();
        let _d = register(&registry, &sink, 1, 4556, Origin::Dialled, node(2)).await;
        let _a = register(&registry, &sink, 2, 50000, Origin::Accepted, node(2)).await;
        let _o = register(&registry, &sink, 3, 50001, Origin::Accepted, node(3)).await;

        assert_eq!(
            sink.claims(),
            HashMap::from([(tcp(4556), vec![node(2)]), (tcp(50001), vec![node(3)])])
        );
    }

    // A dialled pool registering a node takes the adjacency from the
    // node's advertised pool of accepted sessions, adding its own first so
    // the node stays routable throughout.
    #[tokio::test]
    async fn dialled_pool_takes_the_adjacency_from_an_accepted_pool() {
        let sink = Arc::new(ClaimingSink::default());
        let registry = registry();
        let _a = register(&registry, &sink, 2, 50000, Origin::Accepted, node(2)).await;
        assert_eq!(sink.take_calls(), vec![Call::Add(tcp(50000))]);

        let _d = register(&registry, &sink, 1, 4556, Origin::Dialled, node(2)).await;
        assert_eq!(
            sink.take_calls(),
            vec![Call::Add(tcp(4556)), Call::Remove(tcp(50000))]
        );
        assert_eq!(sink.claims(), HashMap::from([(tcp(4556), vec![node(2)])]));
    }

    // When the node's last dialled pool goes, a withheld pool of accepted
    // sessions takes the adjacency over before the dialled one is
    // withdrawn, so the node stays reachable over the sessions it opened.
    #[tokio::test]
    async fn accepted_pool_takes_over_when_the_last_dialled_pool_goes() {
        let sink = Arc::new(ClaimingSink::default());
        let registry = registry();
        let _d = register(&registry, &sink, 1, 4556, Origin::Dialled, node(2)).await;
        let _a = register(&registry, &sink, 2, 50000, Origin::Accepted, node(2)).await;
        sink.take_calls();

        registry.unregister_session(&addr(1), &addr(4556)).await;
        assert_eq!(
            sink.take_calls(),
            vec![Call::Add(tcp(50000)), Call::Remove(tcp(4556))]
        );
        assert_eq!(sink.claims(), HashMap::from([(tcp(50000), vec![node(2)])]));
    }

    // A withheld pool's sessions still carry transfers: the BPA forwards
    // to it no more, but a transfer already handed to it, or a forward
    // racing the withdrawal, completes over its session.
    #[tokio::test]
    async fn withheld_pool_still_carries_transfers() {
        let sink = Arc::new(ClaimingSink::default());
        let registry = registry();
        let _d = register(&registry, &sink, 1, 4556, Origin::Dialled, node(2)).await;
        let rx = register(&registry, &sink, 2, 50000, Origin::Accepted, node(2)).await;
        serve_completed(rx);

        let r = registry
            .forward(
                &addr(50000),
                hardy_bpa::Bytes::from_static(b"bundle"),
                OnBusy::Dial,
            )
            .await;
        assert_eq!(r, Ok(hardy_bpa::cla::TransferOutcome::Completed));
    }

    // A wait for an address's sessions to close returns once the last one
    // has gone, and at once when there is none.
    #[tokio::test]
    async fn sessions_closed_waits_for_the_last_session() {
        let sink = Arc::new(ClaimingSink::default());
        let registry = registry();
        let _a = register(&registry, &sink, 1, 4556, Origin::Dialled, node(2)).await;
        let _b = register(&registry, &sink, 2, 4556, Origin::Dialled, node(2)).await;

        let remote = addr(4556);
        let mut closed = pin!(registry.sessions_closed(&remote));
        assert!(poll!(closed.as_mut()).is_pending());

        registry.unregister_session(&addr(1), &remote).await;
        assert!(poll!(closed.as_mut()).is_pending());

        registry.unregister_session(&addr(2), &remote).await;
        assert!(poll!(closed.as_mut()).is_ready());

        assert!(poll!(pin!(registry.sessions_closed(&remote))).is_ready());
    }

    #[tokio::test]
    async fn idle_session_is_used_and_returned() {
        let (conn, rx) = conn(1);
        serve_completed(rx);

        let pool = ConnectionPool::new(conn, Origin::Dialled, Arc::new(MockSink), addr(4556), 6);
        let r = pool
            .try_send(hardy_bpa::Bytes::from_static(b"bundle"), OnBusy::Dial)
            .await;
        assert!(matches!(r, Ok(hardy_bpa::cla::TransferOutcome::Completed)));

        let inner = pool.inner.lock().unwrap();
        assert_eq!(inner.idle.len(), 1);
        assert!(inner.active.is_empty());
    }
}
