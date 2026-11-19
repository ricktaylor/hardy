use core::time::Duration;

// `closeable::Receiver` is the receiving end of a delivery queue, not the
// stream trait of the same name this module hands to services.
use hardy_async::{
    CancellationToken, TaskPool, async_trait, closeable::Receiver as QueueReceiver,
    sync::spin::Mutex,
};
// `eid::Service` is the service number inside an EID, not the trait of the
// same name this module registers; it is aliased to the word the code
// already uses for it.
use hardy_bpv7::{
    bundle::Id,
    eid::{Eid, Service as ServiceId},
    status_report::ReasonCode,
};
use portable_atomic::{AtomicU32, Ordering};
use time::OffsetDateTime;
use tracing::{error, info};

// This module's own `Sink` implements both service sink traits, so it takes
// the bare name here; the traits stay module-qualified at their few use
// sites.
use super::{BoxFactory, Context, Error, Result, Segment, SendOptions, StatusNotify, raw};
use crate::{
    Arc, Bytes, HashMap, Weak,
    bundle::Bundle,
    dispatcher::Dispatcher,
    hash_map::Entry,
    node_ids::{NodeIds, service_is_admin_endpoint},
    routing::Rib,
    service,
    storage::channel::{SendError, Sender},
    stream::{CancellableReceiver, Receiver},
};

// The service map is behind a spin lock. Its critical section is a lookup,
// the opening of a delivery queue and an insert, all O(1), with the
// service itself built outside it, so the lock is never held long enough
// to repay an OS mutex's park. The queue opens inside because a number
// already taken must leave no queue behind.

/// One registered service: what it was built as, and the queue the BPA
/// delivers to it through.
pub struct Record {
    pub kind: Kind,
    pub service_id: ServiceId,
    // Cancelled at unregistration; every in-flight stream of this
    // registration races it and fails immediately.
    pub cancel: CancellationToken,
    // The canonical registration EID — the exact key the DeliverPending
    // claim, the unregister sweep, and poll_service_waiting match on.
    eid: Eid,
    // Both ends of this registration's delivery queue (the local
    // analogue of a peer's egress queues): the dispatcher fills it
    // through `deliver` and drains it through `next_delivery`.
    tx: Sender,
    rx: QueueReceiver<Bundle>,
}

impl Record {
    /// A registration of `kind` at `eid`, delivered to through `queue`.
    pub fn new(
        kind: Kind,
        service_id: ServiceId,
        eid: Eid,
        (tx, rx): (Sender, QueueReceiver<Bundle>),
    ) -> Arc<Self> {
        Arc::new(Self {
            kind,
            service_id,
            cancel: CancellationToken::new(),
            eid,
            tx,
            rx,
        })
    }

    /// Queues a bundle into this service's delivery channel.
    ///
    /// `Err(bundle)` hands ownership back for parking (the mirror of
    /// `Peer::forward`): the queue has closed (unregistration).
    #[expect(
        clippy::result_large_err,
        reason = "Err(bundle) hands ownership back to the caller; boxing it to shrink the Err variant would allocate on every call"
    )]
    pub async fn deliver(&self, bundle: Bundle) -> core::result::Result<(), Bundle> {
        self.tx.send(bundle).await.map_err(|SendError(b)| b)
    }

    /// Pulls the next bundle queued for delivery, or `None` once the
    /// queue has closed.
    pub async fn next_delivery(&self) -> Option<Bundle> {
        self.rx.recv().await.ok()
    }

    /// The canonical registration EID.
    pub fn eid(&self) -> &Eid {
        &self.eid
    }

    fn close_queue(&self) {
        self.tx.close();
    }
}

impl PartialEq for Record {
    fn eq(&self, other: &Self) -> bool {
        self.service_id == other.service_id
    }
}

impl Eq for Record {}

impl PartialOrd for Record {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Record {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.service_id.cmp(&other.service_id)
    }
}

impl core::hash::Hash for Record {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.service_id.hash(state);
    }
}

impl core::fmt::Debug for Record {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Record")
            .field("eid", &self.eid)
            .finish_non_exhaustive()
    }
}

/// The BPA's implementation of both service sink traits.
// The back-edge is weak. `Bpa` owns the table for the life of the BPA, so
// a strong edge would add no reachability, only a cycle through the
// service that holds this sink, and a cycle of `Arc`s never drops. A
// failed upgrade is the condition the trait already names,
// `Error::Disconnected`.
struct Sink {
    table: Weak<Table>,
    service_id: ServiceId,
    // The registration EID, and the source of every bundle sent here.
    eid: Eid,
}

impl Sink {
    fn new(table: Weak<Table>, service_id: ServiceId, eid: Eid) -> Arc<Self> {
        Arc::new(Self {
            table,
            service_id,
            eid,
        })
    }

    // The registration behind this sink, gone once the service has been
    // unregistered.
    fn record(&self) -> Result<(Arc<Table>, Arc<Record>)> {
        let table = self.table.upgrade().ok_or(Error::Disconnected)?;
        let record = table.record(&self.service_id).ok_or(Error::Disconnected)?;
        Ok((table, record))
    }

    async fn unregister_inner(&self) {
        if let Some(table) = self.table.upgrade() {
            table.unregister(&self.service_id).await;
        }
    }
}

#[async_trait]
impl raw::Sink for Sink {
    async fn unregister(&self) {
        self.unregister_inner().await
    }

    async fn send(&self, stream: &mut dyn Receiver<Segment>) -> Result<Id> {
        let (table, record) = self.record()?;
        let dispatcher = table.dispatcher()?;

        // A service that unregisters mid-send must not originate its
        // bundle: the registration's token races every pull, so teardown
        // wakes this stream immediately, even parked behind a stalled
        // producer, and the send surfaces as cancelled. Sink-side, so the
        // dispatcher's stream consumers stay registration-agnostic.
        let mut stream = CancellableReceiver {
            inner: stream,
            token: record.cancel.clone(),
        };
        dispatcher.originate_raw(&self.eid, &mut stream).await
    }
}

#[async_trait]
impl service::Sink for Sink {
    async fn unregister(&self) {
        self.unregister_inner().await
    }

    async fn send(
        &self,
        destination: Eid,
        data: Bytes,
        lifetime: Duration,
        options: Option<SendOptions>,
    ) -> Result<Id> {
        let (table, _) = self.record()?;
        table
            .dispatcher()?
            .originate(self.eid.clone(), destination, data, lifetime, options)
            .await
    }

    async fn send_streamed(
        &self,
        destination: Eid,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
        lifetime: Duration,
        options: Option<SendOptions>,
    ) -> Result<Id> {
        let (table, record) = self.record()?;
        let dispatcher = table.dispatcher()?;

        // Registration teardown wakes an in-flight send exactly as on the
        // raw door: the token races every pull, sink-side.
        let mut stream = CancellableReceiver {
            inner: stream,
            token: record.cancel.clone(),
        };
        dispatcher
            .originate_streamed(
                self.eid.clone(),
                destination,
                total_len,
                &mut stream,
                lifetime,
                options,
            )
            .await
    }
}

/// Which of the two service contracts a service is declared against.
///
/// The doors differ in the factory they carry and in nothing else, so a
/// declared service is this and its service number.
pub enum FactoryKind<'a> {
    /// A service that receives payloads only.
    Payload(BoxFactory<'a>),
    /// A service that receives whole bundles.
    Raw(raw::BoxFactory<'a>),
}

impl FactoryKind<'_> {
    // Calls the factory this carries, under the contract it names.
    fn build(self, sink: Arc<Sink>, eid: Eid) -> Result<Kind> {
        match self {
            Self::Payload(factory) => Ok(Kind::Payload(factory.build(Context::new(sink, eid))?)),
            Self::Raw(factory) => Ok(Kind::Raw(factory.build(raw::Context::new(sink, eid))?)),
        }
    }
}

/// A registered service, under the contract it was built against.
///
/// Delivery is the only thing the two contracts disagree on, and the
/// dispatcher settles it by what it hands over: a whole bundle on one
/// side, a payload on the other. Everything else is the same call on
/// either, so this answers it.
pub enum Kind {
    /// A service that receives payloads only.
    Payload(Arc<dyn service::Service>),
    /// A service that receives whole bundles.
    Raw(Arc<dyn raw::Service>),
}

impl Kind {
    async fn register(&self) -> Result<()> {
        match self {
            Self::Payload(service) => service.register().await,
            Self::Raw(service) => service.register().await,
        }
    }

    async fn unregister(&self) -> Result<()> {
        match self {
            Self::Payload(service) => service.unregister().await,
            Self::Raw(service) => service.unregister().await,
        }
    }

    /// Passes a status report on to the service it was raised for.
    pub async fn report_status(
        &self,
        bundle_id: &Id,
        from: &Eid,
        kind: StatusNotify,
        reason: ReasonCode,
        timestamp: Option<OffsetDateTime>,
    ) {
        match self {
            Self::Payload(service) => {
                service
                    .report_status(bundle_id, from, kind, reason, timestamp)
                    .await
            }
            Self::Raw(service) => {
                service
                    .report_status(bundle_id, from, kind, reason, timestamp)
                    .await
            }
        }
    }
}

/// Base for dynamically assigned IPN service numbers.
/// Starts high to avoid collisions with explicitly assigned IDs.
const DYNAMIC_SERVICE_BASE: u32 = 0x8000_0000;

// Why a service id is claimed while no registration stands under it: a
// registration whose factory is still running, or the withdrawal of a
// record already taken out of the table.
enum Claim {
    Building,
    Detaching,
}

// What holds a service id. Either a registration stands there or a
// claim does, never both and never neither, so the id's state is the
// slot's variant.
enum Slot {
    Claimed(Claim),
    Attached(Arc<Record>),
}

impl Slot {
    // The registration standing under the id, if the id is not merely
    // claimed: only an attached service is usable.
    fn attached(&self) -> Option<&Arc<Record>> {
        match self {
            Slot::Attached(record) => Some(record),
            Slot::Claimed(_) => None,
        }
    }
}

/// The services of one BPA: the only place a service is built, and the
/// authority on which ones are live.
pub struct Table {
    node_ids: Arc<NodeIds>,
    // The dispatcher owns the BPA's delivery machinery and outlives this
    // table, so the edge back is weak; a failed upgrade is
    // `Error::Disconnected`.
    dispatcher: Weak<Dispatcher>,
    rib: Arc<Rib>,
    services: Mutex<HashMap<ServiceId, Slot>>,
    next_dynamic: AtomicU32,
    tasks: TaskPool,
}

impl Table {
    /// The empty table of one BPA.
    ///
    /// Every service it ever holds arrives through
    /// [`register`](Self::register), the services the embedder declared
    /// no differently from the ones that register while the BPA runs.
    // The dispatcher is the one argument taken by reference: the table
    // holds a `Weak` to it, and keeps the others.
    pub fn new(node_ids: Arc<NodeIds>, dispatcher: &Arc<Dispatcher>, rib: Arc<Rib>) -> Arc<Self> {
        Arc::new(Self {
            node_ids,
            dispatcher: Arc::downgrade(dispatcher),
            rib,
            services: Mutex::new(HashMap::new()),
            next_dynamic: AtomicU32::new(DYNAMIC_SERVICE_BASE),
            tasks: TaskPool::new(),
        })
    }

    pub async fn shutdown(&self) {
        let records = {
            let mut services = self.services.lock();
            let mut records = Vec::new();
            *services = core::mem::take(&mut *services)
                .into_iter()
                .filter_map(|(service_id, slot)| match slot {
                    Slot::Attached(record) => {
                        records.push(record);
                        None
                    }
                    // A registration still inside its factory keeps its
                    // id, claimed for a withdrawal now, so it gives the
                    // service up instead of filing it into a table that
                    // is going away.
                    Slot::Claimed(_) => Some((service_id, Slot::Claimed(Claim::Detaching))),
                })
                .collect();
            records
        };

        if !records.is_empty() {
            metrics::gauge!("bpa.service.registered").decrement(records.len() as f64);
        }

        for record in records {
            self.withdraw(record).await;
        }

        self.tasks.shutdown().await;
    }

    /// Attaches the service and puts it into service: the runtime
    /// door, where the two halves are one call. A service that cannot
    /// run is unregistered again, so a failure leaves nothing behind.
    ///
    /// The service is filed under `id`, or under a freshly allocated
    /// service number when `id` is `None`. The factory is the
    /// embedder's code and may do anything, so it runs outside the
    /// lock, against a reservation of the id: no service is ever built
    /// only to be discarded because another registration took the id,
    /// and none can be reached while it is still being built.
    pub async fn register(
        self: &Arc<Self>,
        id: Option<ServiceId>,
        factory: FactoryKind<'_>,
    ) -> Result<()> {
        // An allocated service number collides with nothing by
        // construction; an explicit one may name the administrative
        // endpoint, which is structurally registered (RFC 9758
        // Section 5.7).
        let service_id =
            id.unwrap_or_else(|| ServiceId::Ipn(self.next_dynamic.fetch_add(1, Ordering::Relaxed)));
        if service_is_admin_endpoint(&service_id) {
            return Err(Error::AdministrativeEndpoint(service_id.to_string()));
        }

        let eid = self.node_ids.resolve_eid(&service_id)?;

        let reservation = self.reserve(&service_id)?;

        // A build that fails drops the reservation, which releases the
        // id: the claim existed only for this build.
        let sink = Sink::new(Arc::downgrade(self), service_id.clone(), eid.clone());
        let service = factory.build(sink, eid.clone())?;

        // The queue's creation-time poll recovers any DeliverPending
        // bundles left over from an earlier registration on this EID;
        // they wait in it until `activate` drains them.
        let record = Record::new(
            service,
            service_id.clone(),
            eid.clone(),
            self.dispatcher()?.new_delivery_channel(&eid),
        );
        reservation.publish(record.clone())?;

        metrics::gauge!("bpa.service.registered").increment(1.0);
        info!("Attached service: {}", record.service_id);

        self.activate(record).await
    }

    /// Unregisters the service at `service_id` and withdraws everything
    /// it published.
    pub async fn unregister(&self, service_id: &ServiceId) {
        let record = {
            let mut services = self.services.lock();
            let Some(slot) = services.get_mut(service_id) else {
                return;
            };

            // The id is claimed for the withdrawal that follows, so
            // nothing registers over a delivery queue still being
            // drained.
            match core::mem::replace(slot, Slot::Claimed(Claim::Detaching)) {
                Slot::Attached(record) => record,
                // A registration still holds the id, and the replace
                // turned its claim into a withdrawal: it reads that once
                // its factory returns and takes the service straight
                // back down instead of filing it. A withdrawal already
                // holding the id reads back what it put there.
                Slot::Claimed(_) => return,
            }
        };

        metrics::gauge!("bpa.service.registered").decrement(1.0);
        self.withdraw(record).await;
        self.services.lock().remove(service_id);
    }

    // Claims `service_id` for a registration, failing if a service or
    // another claim already holds it.
    fn reserve<'a>(&'a self, service_id: &'a ServiceId) -> Result<Reservation<'a>> {
        match self.services.lock().entry(service_id.clone()) {
            Entry::Vacant(slot) => {
                slot.insert(Slot::Claimed(Claim::Building));
                Ok(Reservation {
                    table: self,
                    service_id,
                    claimed: true,
                })
            }
            Entry::Occupied(_) => Err(Error::ServiceIdInUse(service_id.to_string())),
        }
    }

    // The registration at `service_id`, if there is one.
    fn record(&self, service_id: &ServiceId) -> Option<Arc<Record>> {
        self.services
            .lock()
            .get(service_id)
            .and_then(Slot::attached)
            .cloned()
    }

    // Brings an attached service into service: starts it, drains its
    // delivery queue and publishes its route. One that cannot run is
    // unregistered again.
    async fn activate(self: &Arc<Self>, record: Arc<Record>) -> Result<()> {
        let dispatcher = self.dispatcher()?;

        // The service runs before anything can reach it: its queue was
        // opened at attachment, and may already hold bundles the
        // channel's creation-time poll recovered.
        if let Err(e) = record.kind.register().await {
            self.unregister(&record.service_id).await;
            return Err(e);
        }

        dispatcher.start_delivery_queue(record.clone());
        let _ = self
            .rib
            .add_service(record.eid.clone(), record.clone())
            .await;

        // The post-registration poll is spawned, never awaited inline: a
        // service whose event buffer drains only after registration
        // returns, the gRPC-bridge shape, would deadlock registration
        // against its own announcements. Overlapping polls are safe: the
        // poll claims each bundle out of WaitingForService with a status
        // CAS.
        let eid = record.eid.clone();
        hardy_async::spawn!(self.tasks, "post_registration_poll", async move {
            dispatcher.poll_service_waiting(&eid).await;
        });

        info!("Registered service: {}", record.service_id);
        Ok(())
    }

    fn dispatcher(&self) -> Result<Arc<Dispatcher>> {
        self.dispatcher.upgrade().ok_or(Error::Disconnected)
    }

    // Tears a registration down: wake its streams, close its queue,
    // withdraw its route, stop the service.
    async fn withdraw(&self, record: Arc<Record>) {
        // First: wake every in-flight stream of this registration.
        record.cancel.cancel();

        // Close the delivery queue: the consumer finishes its in-flight
        // delivery and exits; queued bundles are swept to WaitingForService
        // by the RIB removal below.
        record.close_queue();

        self.rib.remove_service(&record.eid, record.clone()).await;

        if let Err(e) = record.kind.unregister().await {
            error!("Service {} failed to shut down cleanly: {e}", record.eid);
        }

        info!("Unregistered service: {}", record.eid);
    }
}

// The gauge counts what the table holds: `unregister` takes services
// out one at a time, and dropping the table takes whatever is left,
// which is how a BPA dropped without a shutdown is accounted for.
impl Drop for Table {
    fn drop(&mut self) {
        let attached = self
            .services
            .get_mut()
            .values()
            .filter_map(Slot::attached)
            .count();
        if attached > 0 {
            metrics::gauge!("bpa.service.registered").decrement(attached as f64);
        }
    }
}

// A service id claimed for a registration, held while the embedder's
// factory runs and given up when the registration ends, whichever way
// it ends.
#[must_use = "an unused reservation releases its id immediately"]
struct Reservation<'a> {
    table: &'a Table,
    service_id: &'a ServiceId,
    claimed: bool,
}

impl Reservation<'_> {
    // Files the built record under the reserved id, the only way a
    // service becomes reachable.
    //
    // An `unregister` that arrived while the factory ran turned the
    // claim into a withdrawal, and the record is refused: nothing
    // reached the service and it was never started, so dropping it is
    // the whole teardown.
    fn publish(mut self, record: Arc<Record>) -> Result<()> {
        let mut services = self.table.services.lock();
        self.claimed = false;
        if let Some(slot @ Slot::Claimed(Claim::Building)) = services.get_mut(self.service_id) {
            *slot = Slot::Attached(record);
            return Ok(());
        }

        services.remove(self.service_id);
        Err(Error::Disconnected)
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if self.claimed {
            self.table.services.lock().remove(self.service_id);
        }
    }
}
