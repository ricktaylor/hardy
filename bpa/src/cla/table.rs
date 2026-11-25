use core::num::NonZeroU64;

use hardy_async::{CancellationToken, TaskPool, sync::spin::Mutex};
use hardy_bpv7::{
    bundle::Id,
    eid::{Eid, NodeId},
};
use tracing::{debug, error, info};

// `cla::Cla` and `cla::Sink` are the traits this module's `Record` and
// `Sink` are built around, so both names are taken here; the two traits
// stay module-qualified at their few use sites.
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

// The maps here are behind spin locks. Every critical section is an O(1)
// lookup or insert with nothing awaited, blocked or iterated inside it, so
// the lock is never held long enough to repay an OS mutex's park. In
// particular the embedder's factory runs outside the table's lock,
// against a reservation.

/// One attached CLA: what it was built as, what it was built under, and
/// what it has published.
pub struct Record {
    pub cla: Arc<dyn cla::Cla>,
    pub policy: Arc<dyn FlowControllerFactory>,
    pub terms: Terms,

    name: Arc<str>,
    // Cancelled at detachment; every in-flight stream of this attachment
    // races it and fails immediately.
    cancel: CancellationToken,
    // Keyed by the link-layer adjacency, which is a peer's primary key.
    // The value pairs the node ids known for that peer with its id in the
    // `PeerTable`; an empty list is a neighbour whose EID has not been
    // learned yet, and so has no RIB entry.
    peers: Mutex<HashMap<Address, (Vec<NodeId>, u32)>>,
}

impl Record {
    // Whether `peer_id` is one of this CLA's peers: the ownership check
    // for deferred transfer outcomes.
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

// Why a name is claimed while no record stands under it: a registration
// whose factory is still running, or the teardown of a record already
// taken out of the table.
enum Claim {
    Building,
    Detaching,
}

// What holds a name. Either a record stands there or a claim does,
// never both and never neither, so the name's state is the slot's
// variant.
enum Slot {
    Claimed(Claim),
    Attached(Arc<Record>),
}

impl Slot {
    // The record standing under the name, if the name is not merely
    // claimed: only an attached CLA is usable.
    fn attached(&self) -> Option<&Arc<Record>> {
        match self {
            Slot::Attached(record) => Some(record),
            Slot::Claimed(_) => None,
        }
    }
}

/// The CLAs of one BPA: the only place a CLA is built, and the authority
/// on which ones are live.
pub struct Table {
    node_ids: Arc<NodeIds>,
    // The dispatcher owns this table through its own late-bound cell, so
    // the edge back is weak; a failed upgrade is `Error::Disconnected`.
    dispatcher: Weak<Dispatcher>,
    // The BPA's own reception cap, one half of every settlement.
    max_bundle_size: Option<NonZeroU64>,
    clas: Mutex<HashMap<String, Slot>>,
    // The egress policies configured by CLA name, the embedder's choice
    // rather than the CLA's; a name absent here runs unshaped.
    policies: HashMap<String, Arc<dyn FlowControllerFactory>>,
    rib: Arc<Rib>,
    store: Arc<Store>,
    peers: PeerTable,
    poll_channel_depth: usize,
    tasks: TaskPool,
}

impl Table {
    /// The empty table of one BPA.
    ///
    /// Every CLA it ever holds arrives through
    /// [`register`](Self::register), the CLAs the embedder declared no
    /// differently from the ones that register while the BPA runs.
    // The dispatcher is the one argument taken by reference: the table
    // holds a `Weak` to it, and keeps the others.
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

    /// Attaches the CLA and starts it, the two halves of one call. A
    /// CLA that cannot run is detached again, so a failure leaves
    /// nothing behind.
    ///
    /// The factory is the embedder's code and may do anything, so it runs
    /// outside the lock, against a reservation of the name. No one else
    /// can claim the name while the factory runs, and no one can reach a
    /// CLA that is still being built.
    pub async fn register(self: &Arc<Self>, name: &str, factory: BoxFactory<'_>) -> Result<()> {
        let terms = Terms::negotiate(factory.request(), self.max_bundle_size);
        let policy = self
            .policies
            .get(name)
            .cloned()
            .unwrap_or_else(|| Arc::new(null_policy::FlowControllerFactory::new()));

        let reservation = self.reserve(name)?;

        // A build that fails drops the reservation, which releases the
        // name: the claim existed only for this build.
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
        info!("Attached CLA: {name}");

        if let Err(e) = cla.register().await {
            self.unregister(&name).await;
            return Err(e);
        }
        Ok(())
    }

    /// Unregisters the named CLA and withdraws everything it published.
    ///
    /// Returns whether this call did the detaching: a name that is
    /// absent, already being detached by another caller, or still being
    /// built, answers `false`. In the last case the build itself
    /// completes the detachment.
    pub async fn unregister(&self, name: &str) -> bool {
        let record = {
            let mut clas = self.clas.lock();
            let Some(slot) = clas.get_mut(name) else {
                return false;
            };

            // The name is claimed for the teardown that follows, so
            // nothing re-registers over peers still being withdrawn.
            match core::mem::replace(slot, Slot::Claimed(Claim::Detaching)) {
                Slot::Attached(record) => record,
                // A registration still holds the name, and the replace
                // turned its claim into a teardown: it reads that once
                // its factory returns and takes the CLA straight back
                // down instead of filing it. A teardown already holding
                // the name reads back what it put there.
                Slot::Claimed(_) => return false,
            }
        };

        // First: wake every in-flight stream of this attachment.
        record.cancel.cancel();

        if let Err(e) = record.cla.unregister().await {
            error!("CLA {name} failed to shut down cleanly: {e}");
        }

        let peers = core::mem::take(&mut *record.peers.lock());
        for (_, (node_ids, peer_id)) in peers {
            // Close the peer's egress queues first, then withdraw its RIB
            // entries: the ForwardPending/ForwardAckPending sweeps inside
            // remove_forward must run once no new send can re-enter the
            // queue (mirrors remove_peer). A send that lands after the
            // close bounces and is parked back to Waiting by its caller.
            self.peers.remove(peer_id).await;
            for node_id in node_ids {
                self.rib.remove_forward(node_id, peer_id).await;
                metrics::gauge!("bpa.fib.entries", "cla" => record.name.clone()).decrement(1.0);
            }
        }

        self.clas.lock().remove(name);
        metrics::gauge!("bpa.cla.registered").decrement(1.0);
        info!("Detached CLA: {name}");

        true
    }

    pub async fn shutdown(&self) {
        // Every claim becomes a teardown as well, so a registration
        // still running its factory gives its name up instead of filing
        // a CLA into a table that is going away.
        let names = {
            let mut clas = self.clas.lock();
            clas.iter_mut()
                .filter_map(|(name, slot)| match slot {
                    Slot::Attached(_) => Some(name.clone()),
                    Slot::Claimed(claim) => {
                        *claim = Claim::Detaching;
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

    // Claims `name` for a registration, failing if a CLA or another
    // claim already holds it.
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
                Slot::Claimed(Claim::Detaching) => Error::Unregistering(name.to_owned()),
            }),
        }
    }

    // The record filed under `name`, if one is attached. A name a build
    // still holds, and one a detaching record still holds, both answer
    // `None`: only an attached CLA is usable.
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
        // Mint the id without publishing anything (reserved against reuse),
        // then claim the address, the adjacency's natural key, so a
        // duplicate exits before any peer state exists.
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

        // Construct the peer complete, then publish: the table only ever
        // holds working peers.
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

        // Post-construction liveness re-check: a concurrent
        // remove_peer/detach during construction has already taken the
        // address entry, so withdraw the published peer instead of
        // installing RIB entries nothing would ever clean up.
        let still_ours =
            matches!(record.peers.lock().get(&cla_addr), Some((_, id)) if *id == peer_id);
        if !still_ours {
            self.peers.remove(peer_id).await;
            return false;
        }

        // A neighbour, one whose node ids are not known, gets no RIB
        // entry, and nothing supplies them later.
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

// The gauge counts what the table holds: `unregister` takes CLAs out
// one at a time, and dropping the table takes whatever is left, which
// is how a BPA dropped without a shutdown is accounted for.
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

// A name claimed for a registration, held while the embedder's factory
// runs and given up when the registration ends, whichever way it ends.
#[must_use = "an unused reservation releases its name immediately"]
struct Reservation<'a> {
    table: &'a Table,
    name: &'a str,
    claimed: bool,
}

impl Reservation<'_> {
    // Files the built record under the reserved name, the only way a
    // CLA becomes reachable.
    //
    // An `unregister` that arrived while the factory ran turned the claim
    // into a teardown, and the record is refused: nothing was published
    // and the CLA was never started, so dropping it is the whole
    // teardown.
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

// A CLA is named, not pointed at: the sink holds its key and a weak,
// non-owning edge to the table, and re-resolves on every operation. The
// table is then the single authority on liveness, and a failed resolution
// is the condition the trait already names, `Error::Disconnected`.
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

        // A CLA that detaches mid-stream must not land its bundle: the
        // attachment's token races every pull, so teardown wakes this
        // stream immediately, even parked behind a stalled producer, and
        // the transfer surfaces as cancelled. Sink-side, so the dispatcher's
        // stream consumers stay attachment-agnostic.
        let mut stream = CancellableReceiver {
            inner: stream,
            token: record.cancel.clone(),
        };
        let verdict = dispatcher
            .receive_bundle(self.name.clone(), peer_node, peer_addr, &mut stream)
            .await;

        // A refusal caused by that teardown is not a verdict on the
        // bundle: the attachment died. Report the dead sink instead, so the
        // CLA does not mistake its own detachment for a per-bundle refusal.
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

    // A CLA that does nothing but keep the sink it was built with, so the
    // tests can drive the BPA from the CLA's side of the contract.
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

    // A name is exclusive: the second CLA to claim one is refused.
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

    // The embedder's factory is not called under the table's lock. A
    // factory may log, allocate or reach back into the table, any of
    // which would spin against a lock `attach` was still holding.
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
                        .expect("The table's lock is held while the factory runs");
                    matches!(clas.get("probe-cla"), Some(Slot::Claimed(Claim::Building)))
                };
                // The name is reserved for the duration, so nothing else
                // can claim it while this runs.
                assert!(reserved, "The name is not reserved while the factory runs");
                Ok(Arc::new(TestCla { sink: cx.sink }))
            }),
        )
        .await
        .expect("Registration should succeed");

        bpa.shutdown().await;
    }

    // A factory that fails leaves nothing behind, so the name it
    // reserved is free for the next attempt.
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

    // Adding a peer installs a RIB entry; removing it withdraws it.
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

    // Detaching a CLA takes its peers with it, and releases its name.
    #[tokio::test]
    async fn detaching_releases_peers_and_name() {
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
            "Re-registration after detaching should succeed, got: {}",
            result.err().unwrap()
        );

        bpa.shutdown().await;
    }
}
