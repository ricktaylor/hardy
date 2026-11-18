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
// the lock is never held long enough to repay an OS mutex's park.

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

// Where a record stands. `Attached` is usable; `Detaching` marks the
// window between `detach` taking the record and the teardown it awaits
// completing, during which the record is published but no longer usable,
// and holds its name against reuse.
enum State {
    Attached,
    Detaching,
}

struct Slot {
    record: Arc<Record>,
    state: State,
}

/// The CLAs of one BPA: the only place a CLA is built, and the authority
/// on which ones are live.
pub struct Table {
    node_ids: Arc<NodeIds>,
    // The dispatcher owns this table through its own late-bound cell, so
    // the edge back is weak; a failed upgrade is `Error::Disconnected`.
    dispatcher: Weak<Dispatcher>,
    // The BPA's own reception cap, one half of every settlement.
    max_reception_size: Option<NonZeroU64>,
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
    /// The table of one BPA, holding the CLAs `declared` names, each built
    /// and filed inert.
    ///
    /// Building them here rather than afterwards leaves no moment at which
    /// the table is live but incomplete, and keeps attachment the table's
    /// own business: a CLA declared at build time and one registered at
    /// runtime take the same path.
    // The dispatcher is the one argument taken by reference: the table
    // holds a `Weak` to it, and keeps the others.
    pub fn new(
        node_ids: Arc<NodeIds>,
        dispatcher: &Arc<Dispatcher>,
        rib: Arc<Rib>,
        store: Arc<Store>,
        poll_channel_depth: usize,
        policies: HashMap<String, Arc<dyn FlowControllerFactory>>,
        declared: Vec<(String, BoxFactory<'static>)>,
    ) -> Result<Arc<Self>> {
        let table = Arc::new(Self {
            node_ids,
            dispatcher: Arc::downgrade(dispatcher),
            max_reception_size: dispatcher.max_bundle_size(),
            clas: Mutex::new(HashMap::new()),
            policies,
            rib,
            store,
            peers: PeerTable::new(),
            poll_channel_depth,
            tasks: TaskPool::new(),
        });

        for (name, factory) in declared {
            table.attach(&name, factory)?;
        }
        Ok(table)
    }

    // Builds the CLA `factory` makes and files it under `name`, inert.
    //
    // Negotiating, minting the context and calling the factory involve no
    // awaits, so the whole operation runs under one lock and no CLA is
    // ever half-attached. The build-time door stops here; the runtime
    // door continues into `register`.
    fn attach(self: &Arc<Self>, name: &str, factory: BoxFactory<'_>) -> Result<Arc<dyn cla::Cla>> {
        let terms = Terms::negotiate(factory.request(), self.max_reception_size);
        let policy = self
            .policies
            .get(name)
            .cloned()
            .unwrap_or_else(|| Arc::new(null_policy::FlowControllerFactory::new()));

        let mut clas = self.clas.lock();
        let slot = match clas.entry(name.to_owned()) {
            Entry::Vacant(slot) => slot,
            Entry::Occupied(slot) => {
                return Err(match slot.get().state {
                    State::Attached => Error::DuplicateName(name.to_owned()),
                    State::Detaching => Error::Detaching(name.to_owned()),
                });
            }
        };

        let name: Arc<str> = Arc::from(name);
        let cla = factory.build(Context::new(
            Arc::new(Sink {
                table: Arc::downgrade(self),
                name: name.clone(),
            }),
            self.node_ids.clone(),
            terms,
        ))?;

        slot.insert(Slot {
            record: Arc::new(Record {
                cla: cla.clone(),
                policy,
                terms,
                name: name.clone(),
                cancel: CancellationToken::new(),
                peers: Mutex::new(HashMap::new()),
            }),
            state: State::Attached,
        });
        drop(clas);

        metrics::gauge!("bpa.cla.registered").increment(1.0);
        info!("Attached CLA: {name}");

        Ok(cla)
    }

    /// Attaches the CLA and starts it: the runtime door, where the two
    /// halves are one call. A CLA that cannot run is detached again, so a
    /// failure leaves nothing behind.
    pub async fn register(self: &Arc<Self>, name: &str, factory: BoxFactory<'_>) -> Result<()> {
        let cla = self.attach(name, factory)?;
        if let Err(e) = cla.start().await {
            self.detach(name).await;
            return Err(e);
        }
        Ok(())
    }

    /// Starts every attached CLA, in attachment order, and reports those
    /// that could not run at all; each of those is detached first, so the
    /// table holds only working CLAs when this returns.
    ///
    /// Called by `Bpa::start` once storage recovery has completed: a CLA
    /// opening its listeners earlier would race the consistency check.
    pub async fn start(&self) -> Vec<(String, Error)> {
        let attached = self
            .clas
            .lock()
            .iter()
            .filter(|(_, slot)| matches!(slot.state, State::Attached))
            .map(|(name, slot)| (name.clone(), slot.record.cla.clone()))
            .collect::<Vec<_>>();

        let mut failures = Vec::new();
        for (name, cla) in attached {
            if let Err(e) = cla.start().await {
                error!("CLA {name} failed to start: {e}");
                self.detach(&name).await;
                failures.push((name, e));
            }
        }
        failures
    }

    /// Tears the named CLA down and withdraws everything it published.
    ///
    /// Returns whether this call did the detaching: a name that is absent,
    /// or already being detached by another caller, answers `false`.
    pub async fn detach(&self, name: &str) -> bool {
        let record = {
            let mut clas = self.clas.lock();
            let Some(slot) = clas.get_mut(name) else {
                return false;
            };
            if matches!(slot.state, State::Detaching) {
                return false;
            }
            slot.state = State::Detaching;
            slot.record.clone()
        };

        // First: wake every in-flight stream of this attachment.
        record.cancel.cancel();

        if let Err(e) = record.cla.shutdown().await {
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
        let names = self.clas.lock().keys().cloned().collect::<Vec<_>>();
        for name in names {
            self.detach(&name).await;
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

    // The record filed under `name`, if one is attached. A detaching
    // record is already unusable, so it answers `None`.
    fn record(&self, name: &str) -> Option<Arc<Record>> {
        self.clas
            .lock()
            .get(name)
            .and_then(|slot| match slot.state {
                State::Attached => Some(slot.record.clone()),
                State::Detaching => None,
            })
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
            table.detach(&self.name).await;
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
        let bpa = Bpa::builder().build().await.unwrap();
        assert!(bpa.start(false).await.is_empty());

        bpa.register_cla("test-cla", TestCla::factory())
            .await
            .expect("First CLA registration should succeed");

        let Err(err) = bpa.register_cla("test-cla", TestCla::factory()).await else {
            panic!("A second CLA claiming the name should be refused");
        };
        assert!(matches!(err, Error::DuplicateName(ref name) if name == "test-cla"));

        bpa.shutdown().await;
    }

    // Adding a peer installs a RIB entry; removing it withdraws it.
    #[tokio::test]
    async fn peer_lifecycle() {
        let bpa = Bpa::builder().build().await.unwrap();
        assert!(bpa.start(false).await.is_empty());

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
        let bpa = Bpa::builder().build().await.unwrap();
        assert!(bpa.start(false).await.is_empty());

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
