use core::num::NonZeroU64;

use hardy_async::{CancellationToken, TaskPool, sync::spin::Mutex};
use hardy_bpv7::{
    bundle::Id,
    eid::{Eid, NodeId},
};
use tracing::{debug, error, info};

// `Cla` and `Sink` name items in this module, so the traits of the same
// name are used qualified, as `cla::Cla` and `cla::Sink`.
use super::{
    Acceptance, Address, BoxFactory, Context, Error, Result, Segment, Terms, TransferOutcome,
    peers::{Peer, PeerTable},
};
use crate::{
    Arc, HashMap, Weak, async_trait,
    bundle::Bundle,
    cla,
    dispatcher::Dispatcher,
    hash_map::Entry,
    node_ids::NodeIds,
    policy::{FlowControllerFactory, null_policy},
    routing::Rib,
    storage::store::Store,
    stream::{CancellableReceiver, Receiver},
};

// The maps are behind spin locks. Every critical section is an O(1) map
// operation, and nothing is awaited while a lock is held; the embedder's
// factory runs outside the table's lock, against a reservation.

/// A registered CLA and the state the table keeps for it.
pub struct Record {
    pub cla: Arc<dyn cla::Cla>,
    pub policy: Arc<dyn FlowControllerFactory>,
    pub terms: Terms,

    name: Arc<str>,
    cancel: CancellationToken,
    // The node ids known for the peer, empty where its EID is unknown, and
    // its id in the `PeerTable`.
    peers: Mutex<HashMap<Address, (Vec<NodeId>, u32)>>,
}

impl Record {
    // Deferred transfer outcomes are accepted only from the CLA that owns
    // the peer.
    pub fn owns_peer(&self, peer_id: u32) -> bool {
        self.peers.lock().values().any(|(_, id)| *id == peer_id)
    }
}

impl core::fmt::Debug for Record {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Record")
            .field("name", &self.name)
            .field("terms", &self.terms)
            .field("peers", &self.peers)
            .finish_non_exhaustive()
    }
}

// A name is claimed, rather than occupied by a record, while its
// registration builds and while its record is torn down.
enum Claim {
    Building,
    Unregistering,
}

// The occupant of a name: a registered CLA, or a claim on the name.
enum Slot {
    Claimed(Claim),
    Attached(Arc<Record>),
}

impl Slot {
    fn attached(&self) -> Option<&Arc<Record>> {
        match self {
            Slot::Attached(record) => Some(record),
            Slot::Claimed(_) => None,
        }
    }
}

/// The registered CLAs of a BPA.
pub struct Table {
    node_ids: Arc<NodeIds>,
    // The dispatcher owns this table, so the reference back is weak. A
    // failed upgrade maps to `Error::Disconnected`.
    dispatcher: Weak<Dispatcher>,
    max_bundle_size: Option<NonZeroU64>,
    clas: Mutex<HashMap<String, Slot>>,
    policies: HashMap<String, Arc<dyn FlowControllerFactory>>,
    rib: Arc<Rib>,
    store: Arc<Store>,
    peers: PeerTable,
    poll_channel_depth: usize,
    tasks: TaskPool,
}

impl Table {
    /// Creates an empty table. CLAs are added by
    /// [`register`](Self::register) only.
    // `dispatcher` is borrowed because the table keeps only a `Weak` to it.
    pub fn new(
        node_ids: Arc<NodeIds>,
        dispatcher: &Arc<Dispatcher>,
        rib: Arc<Rib>,
        store: Arc<Store>,
        poll_channel_depth: usize,
        policies: HashMap<String, Arc<dyn FlowControllerFactory>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            node_ids,
            dispatcher: Arc::downgrade(dispatcher),
            max_bundle_size: dispatcher.max_bundle_size(),
            clas: Mutex::new(HashMap::new()),
            policies,
            rib,
            store,
            peers: PeerTable::new(),
            poll_channel_depth,
            tasks: TaskPool::new(),
        })
    }

    /// Builds the CLA, files it under `name`, and calls
    /// [`Cla::register`](cla::Cla::register).
    ///
    /// The factory runs outside the table's lock, against a reservation of
    /// `name`, so no other registration can take the name and no caller can
    /// reach a CLA that is still being built.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DuplicateName`] or [`Error::Unregistering`] if
    /// `name` is taken, or the error returned by the factory or by
    /// [`Cla::register`](cla::Cla::register). A CLA whose `register` fails
    /// is unregistered again, so a failed call registers nothing.
    pub async fn register(self: &Arc<Self>, name: &str, factory: BoxFactory<'_>) -> Result<()> {
        let terms = Terms::negotiate(factory.request(), self.max_bundle_size);
        let policy = self
            .policies
            .get(name)
            .cloned()
            .unwrap_or_else(|| Arc::new(null_policy::FlowControllerFactory::new()));

        let reservation = self.reserve(name)?;

        // Dropping the reservation releases the name.
        let name: Arc<str> = Arc::from(name);
        let cla = factory.build(Context::new(
            Arc::new(Sink {
                table: Arc::downgrade(self),
                name: name.clone(),
            }),
            self.node_ids.clone(),
            terms,
        ))?;
        reservation.publish(Arc::new(Record {
            cla: cla.clone(),
            policy,
            terms,
            name: name.clone(),
            cancel: CancellationToken::new(),
            peers: Mutex::new(HashMap::new()),
        }))?;

        metrics::gauge!("bpa.cla.registered").increment(1.0);
        info!("Registered CLA: {name}");

        if let Err(e) = cla.register().await {
            self.unregister(&name).await;
            return Err(e);
        }
        Ok(())
    }

    /// Unregisters the CLA filed under `name` and withdraws its peers and
    /// their routes.
    ///
    /// Returns `true` if this call performed the unregistration, and
    /// `false` if `name` is absent, already unregistering, or still
    /// building. A CLA that is still building is torn down by
    /// [`register`](Self::register) instead.
    pub async fn unregister(&self, name: &str) -> bool {
        let record = {
            let mut clas = self.clas.lock();
            let Some(slot) = clas.get_mut(name) else {
                return false;
            };

            // Claim the name for the teardown, so that nothing registers
            // over peers that are still being withdrawn.
            match core::mem::replace(slot, Slot::Claimed(Claim::Unregistering)) {
                Slot::Attached(record) => record,
                // A registration holds the name. The claim just written
                // tells it to tear the CLA down instead of filing it.
                Slot::Claimed(_) => return false,
            }
        };

        // Cancel the in-flight streams before calling into the CLA.
        record.cancel.cancel();

        if let Err(e) = record.cla.unregister().await {
            error!("CLA {name} failed to shut down cleanly: {e}");
        }

        let peers = core::mem::take(&mut *record.peers.lock());
        for (_, (node_ids, peer_id)) in peers {
            // Close the egress queues before withdrawing the RIB entries,
            // as `remove_peer` does: the sweeps in `remove_forward` require
            // that no further send can enter the queue. A send that arrives
            // after the close is parked back to Waiting by its caller.
            self.peers.remove(peer_id).await;
            for node_id in node_ids {
                self.rib.remove_forward(node_id, peer_id).await;
                metrics::gauge!("bpa.fib.entries", "cla" => record.name.clone()).decrement(1.0);
            }
        }

        self.clas.lock().remove(name);
        metrics::gauge!("bpa.cla.registered").decrement(1.0);
        info!("Unregistered CLA: {name}");

        true
    }

    pub async fn shutdown(&self) {
        // Outstanding claims become teardowns, so a registration still
        // inside its factory releases its name instead of filing into a
        // table that is shutting down.
        let names = {
            let mut clas = self.clas.lock();
            clas.iter_mut()
                .filter_map(|(name, slot)| match slot {
                    Slot::Attached(_) => Some(name.clone()),
                    Slot::Claimed(claim) => {
                        *claim = Claim::Unregistering;
                        None
                    }
                })
                .collect::<Vec<_>>()
        };

        for name in names {
            self.unregister(&name).await;
        }

        self.tasks.shutdown().await;
    }

    #[expect(
        clippy::result_large_err,
        reason = "Err(bundle) hands ownership back to the caller; boxing it to shrink the Err variant would allocate on every call"
    )]
    pub async fn forward(
        &self,
        peer_id: u32,
        next_hop: Eid,
        bundle: Bundle,
    ) -> core::result::Result<(), Bundle> {
        self.peers.forward(peer_id, next_hop, bundle).await
    }

    // Claims `name`, failing if a CLA or another claim holds it.
    fn reserve<'a>(&'a self, name: &'a str) -> Result<Reservation<'a>> {
        match self.clas.lock().entry(name.to_owned()) {
            Entry::Vacant(slot) => {
                slot.insert(Slot::Claimed(Claim::Building));
                Ok(Reservation {
                    table: self,
                    name,
                    claimed: true,
                })
            }
            Entry::Occupied(slot) => Err(match slot.get() {
                Slot::Attached(_) | Slot::Claimed(Claim::Building) => {
                    Error::DuplicateName(name.to_owned())
                }
                Slot::Claimed(Claim::Unregistering) => Error::Unregistering(name.to_owned()),
            }),
        }
    }

    // A name held only by a claim has no record.
    fn record(&self, name: &str) -> Option<Arc<Record>> {
        self.clas.lock().get(name).and_then(Slot::attached).cloned()
    }

    async fn add_peer(
        &self,
        record: &Arc<Record>,
        dispatcher: Arc<Dispatcher>,
        cla_addr: Address,
        node_ids: &[NodeId],
    ) -> bool {
        // Reserve the id, then claim the address, so that a duplicate
        // address returns before any peer state exists.
        let reservation = self.peers.reserve();
        let peer_id = reservation.id();
        let claimed = {
            let mut peers = record.peers.lock();
            match peers.entry(cla_addr.clone()) {
                Entry::Vacant(e) => {
                    e.insert((node_ids.to_vec(), peer_id));
                    true
                }
                Entry::Occupied(_) => false,
            }
        };
        if !claimed {
            return false;
        }

        let cla_name = record.name.clone();

        debug!("Added new peer {peer_id}: [{node_ids:?}] at {cla_addr} via CLA {cla_name}");

        // Publish only once the peer is running.
        let peer = Peer::start(
            self.poll_channel_depth,
            record.clone(),
            peer_id,
            cla_addr.clone(),
            self.store.clone(),
            dispatcher,
            &self.tasks,
        )
        .await;
        reservation.publish(peer);

        // A concurrent `remove_peer` or unregistration has taken the
        // address entry, so withdraw the peer instead of installing RIB
        // entries that nothing removes.
        let still_ours =
            matches!(record.peers.lock().get(&cla_addr), Some((_, id)) if *id == peer_id);
        if !still_ours {
            self.peers.remove(peer_id).await;
            return false;
        }

        // An empty `node_ids` means the peer's EID is unknown: no RIB entry
        // is installed, and none is added later.
        for node_id in node_ids {
            let _ = self.rib.add_forward(node_id.clone(), peer_id).await;
            metrics::gauge!("bpa.fib.entries", "cla" => cla_name.clone()).increment(1.0);
        }

        true
    }

    async fn remove_peer(&self, record: &Arc<Record>, cla_addr: &Address) -> bool {
        let Some((node_ids, peer_id)) = record.peers.lock().remove(cla_addr) else {
            return false;
        };

        self.peers.remove(peer_id).await;
        for node_id in node_ids {
            self.rib.remove_forward(node_id, peer_id).await;
            metrics::gauge!("bpa.fib.entries", "cla" => record.name.clone()).decrement(1.0);
        }

        debug!("Removed peer {peer_id}");

        true
    }
}

// `unregister` decrements the gauge per CLA; this accounts for the CLAs
// still registered when the table is dropped.
impl Drop for Table {
    fn drop(&mut self) {
        let attached = self
            .clas
            .get_mut()
            .values()
            .filter_map(Slot::attached)
            .count();
        if attached > 0 {
            metrics::gauge!("bpa.cla.registered").decrement(attached as f64);
        }
    }
}

// A claim on a name, released on drop unless `publish` consumes it.
#[must_use = "an unused reservation releases its name immediately"]
struct Reservation<'a> {
    table: &'a Table,
    name: &'a str,
    claimed: bool,
}

impl Reservation<'_> {
    // Files `record` under the reserved name.
    //
    // Fails with `Error::Unregistering` if an `unregister` arrived while the
    // factory ran. The record is then dropped having never been published
    // or started, which completes the teardown.
    fn publish(mut self, record: Arc<Record>) -> Result<()> {
        let mut clas = self.table.clas.lock();
        self.claimed = false;
        if let Some(slot @ Slot::Claimed(Claim::Building)) = clas.get_mut(self.name) {
            *slot = Slot::Attached(record);
            return Ok(());
        }

        clas.remove(self.name);
        Err(Error::Unregistering(self.name.to_owned()))
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if self.claimed {
            self.table.clas.lock().remove(self.name);
        }
    }
}

// Holds the CLA's name and a weak reference to the table, and re-resolves
// on each call, so the table remains the only authority on liveness. A
// failed resolution maps to `Error::Disconnected`.
struct Sink {
    table: Weak<Table>,
    name: Arc<str>,
}

impl Sink {
    fn resolve(&self) -> Result<(Arc<Table>, Arc<Record>)> {
        let table = self.table.upgrade().ok_or(Error::Disconnected)?;
        let record = table.record(&self.name).ok_or(Error::Disconnected)?;
        Ok((table, record))
    }

    fn dispatcher(table: &Table) -> Result<Arc<Dispatcher>> {
        table.dispatcher.upgrade().ok_or(Error::Disconnected)
    }
}

#[async_trait]
impl cla::Sink for Sink {
    async fn unregister(&self) {
        if let Some(table) = self.table.upgrade() {
            table.unregister(&self.name).await;
        }
    }

    async fn dispatch(
        &self,
        peer_node: Option<&NodeId>,
        peer_addr: Option<&Address>,
        stream: &mut dyn Receiver<Segment>,
    ) -> Result<Acceptance> {
        let (table, record) = self.resolve()?;
        let dispatcher = Self::dispatcher(&table)?;

        // Cancel the stream if the CLA unregisters: the token is raced
        // against every pull, so a teardown wakes a stream parked behind a
        // stalled producer. Done here to keep the dispatcher's stream
        // consumers independent of registration state.
        let mut stream = CancellableReceiver {
            inner: stream,
            token: record.cancel.clone(),
        };
        let verdict = dispatcher
            .receive_bundle(self.name.clone(), peer_node, peer_addr, &mut stream)
            .await;

        // A refusal caused by the teardown is not a verdict on the bundle,
        // so report the disconnected sink instead.
        if matches!(verdict, Acceptance::Refused) && record.cancel.is_cancelled() {
            return Err(Error::Disconnected);
        }
        Ok(verdict)
    }

    async fn add_peer(&self, cla_addr: Address, node_ids: &[NodeId]) -> Result<bool> {
        let (table, record) = self.resolve()?;
        let dispatcher = Self::dispatcher(&table)?;
        Ok(table
            .add_peer(&record, dispatcher, cla_addr, node_ids)
            .await)
    }

    async fn remove_peer(&self, cla_addr: &Address) -> Result<bool> {
        let (table, record) = self.resolve()?;
        Ok(table.remove_peer(&record, cla_addr).await)
    }

    async fn report_transfer(&self, bundle_id: &Id, outcome: TransferOutcome) -> Result<()> {
        let (table, record) = self.resolve()?;
        let dispatcher = Self::dispatcher(&table)?;
        dispatcher
            .transfer_outcome(&record, bundle_id, outcome)
            .await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use hardy_bpv7::{
        bundle::Id,
        eid::{IpnNodeId, NodeId},
    };

    use super::{Claim, Slot};
    use crate::{
        Arc, async_trait,
        bpa::Bpa,
        cla::{
            self, Address, Context, Error, Factory, ForwardOutcome, RegistrarExt, Request, Result,
            Segment, Sink, factory_fn,
        },
        stream::Receiver,
    };

    // A CLA that keeps only its sink, so that the tests can drive the BPA
    // through the CLA side of the contract.
    struct TestCla {
        sink: Arc<dyn Sink>,
    }

    impl TestCla {
        fn factory() -> impl Factory<Component = Self> {
            factory_fn(Request::default(), |cx: Context| {
                Ok(Arc::new(Self { sink: cx.sink }))
            })
        }
    }

    #[async_trait]
    impl cla::Cla for TestCla {
        async fn forward(
            &self,
            _lane: Option<u32>,
            _cla_addr: &Address,
            _bundle_id: &Id,
            _total_len: u64,
            _stream: &mut dyn Receiver<Segment>,
        ) -> Result<ForwardOutcome> {
            Ok(ForwardOutcome::Sent)
        }
    }

    #[tokio::test]
    async fn duplicate_name_is_refused() {
        let bpa = Bpa::builder().build();
        bpa.start(false).await.unwrap();

        bpa.register_cla("test-cla", TestCla::factory())
            .await
            .expect("First CLA registration should succeed");

        let Err(err) = bpa.register_cla("test-cla", TestCla::factory()).await else {
            panic!("A second CLA claiming the name should be refused");
        };
        assert!(matches!(err, Error::DuplicateName(ref name) if name == "test-cla"));

        bpa.shutdown().await;
    }

    // The factory must not run under the table's lock: a factory is free to
    // call back into the table.
    #[tokio::test]
    async fn the_factory_runs_outside_the_table_lock() {
        let bpa = Bpa::builder().build();
        bpa.start(false).await.unwrap();

        let probe = Arc::downgrade(bpa.cla_table());
        bpa.register_cla(
            "probe-cla",
            factory_fn(Request::default(), move |cx: Context| {
                let table = probe.upgrade().expect("The table outlives the build");
                let reserved = {
                    let clas = table
                        .clas
                        .try_lock()
                        .expect("The table's lock is free while the factory runs");
                    matches!(clas.get("probe-cla"), Some(Slot::Claimed(Claim::Building)))
                };
                assert!(reserved, "The name is not reserved while the factory runs");
                Ok(Arc::new(TestCla { sink: cx.sink }))
            }),
        )
        .await
        .expect("Registration should succeed");

        bpa.shutdown().await;
    }

    #[tokio::test]
    async fn a_failed_build_releases_the_name() {
        let bpa = Bpa::builder().build();
        bpa.start(false).await.unwrap();

        let Err(err) = bpa
            .register_cla(
                "flaky-cla",
                factory_fn(Request::default(), |_cx: Context| -> Result<Arc<TestCla>> {
                    Err(Error::Internal("The factory refused".into()))
                }),
            )
            .await
        else {
            panic!("A factory that fails should fail the registration");
        };
        assert!(matches!(err, Error::Internal(_)));

        bpa.register_cla("flaky-cla", TestCla::factory())
            .await
            .expect("The refused name should be free again");

        bpa.shutdown().await;
    }

    // Adding a peer installs a RIB entry, and removing it withdraws one.
    #[tokio::test]
    async fn peer_lifecycle() {
        let bpa = Bpa::builder().build();
        bpa.start(false).await.unwrap();

        let cla = bpa
            .register_cla("lifecycle-cla", TestCla::factory())
            .await
            .unwrap();

        let peer_addr = Address::Private("peer1".as_bytes().into());
        let peer_node = NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 10,
        });

        let added = cla
            .sink
            .add_peer(peer_addr.clone(), core::slice::from_ref(&peer_node))
            .await
            .unwrap();
        assert!(added, "First add_peer should succeed");

        let removed = cla.sink.remove_peer(&peer_addr).await.unwrap();
        assert!(removed, "remove_peer should succeed");

        let removed = cla.sink.remove_peer(&peer_addr).await.unwrap();
        assert!(!removed, "Double remove_peer should return false");

        bpa.shutdown().await;
    }

    #[tokio::test]
    async fn unregistering_releases_peers_and_name() {
        let bpa = Bpa::builder().build();
        bpa.start(false).await.unwrap();

        let cla = bpa
            .register_cla("cascade-cla", TestCla::factory())
            .await
            .unwrap();

        let addr1 = Address::Private("p1".as_bytes().into());
        let addr2 = Address::Private("p2".as_bytes().into());
        let node1 = NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 20,
        });
        let node2 = NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: 21,
        });

        cla.sink.add_peer(addr1, &[node1]).await.unwrap();
        cla.sink.add_peer(addr2, &[node2]).await.unwrap();

        cla.sink.unregister().await;

        let result = bpa.register_cla("cascade-cla", TestCla::factory()).await;
        assert!(
            result.is_ok(),
            "Re-registration after unregistering should succeed, got: {}",
            result.err().unwrap()
        );

        bpa.shutdown().await;
    }
}
