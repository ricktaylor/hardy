use core::time::Duration;

// `Receiver` names the stream trait this module hands to services, so the
// receiving end of a delivery queue is aliased.
use hardy_async::{
    CancellationToken, TaskPool, async_trait, closeable::Receiver as QueueReceiver,
    sync::spin::Mutex,
};
// `eid::Service` is a service number, not the service trait, so it is
// aliased to the word the rest of the code uses for it.
use hardy_bpv7::{
    bundle::Id,
    eid::{Eid, Service as ServiceId},
    status_report::ReasonCode,
};
use portable_atomic::{AtomicU32, Ordering};
use time::OffsetDateTime;
use tracing::{error, info};

// `Sink` names a type in this module, so the two sink traits it implements
// are used qualified, as `service::Sink` and `raw::Sink`.
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

// The map is behind a spin lock. Every critical section is an O(1) map
// operation, and nothing is awaited while the lock is held; the embedder's
// factory runs outside the table's lock, against a reservation.

/// A registered service and the queue the BPA delivers to it through.
pub struct Record {
    pub kind: Kind,
    pub service_id: ServiceId,
    pub cancel: CancellationToken,
    // The registration's canonical EID. The DeliverPending claim, the
    // unregister sweep and `poll_service_waiting` all match on it.
    eid: Eid,
    tx: Sender,
    rx: QueueReceiver<Bundle>,
}

impl Record {
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

    /// Queues a bundle for delivery to this service.
    ///
    /// Returns the bundle if the queue has closed, which happens once the
    /// service unregisters. The caller parks what comes back, as
    /// `Peer::forward` does.
    #[expect(
        clippy::result_large_err,
        reason = "Err(bundle) hands ownership back to the caller; boxing it to shrink the Err variant would allocate on every call"
    )]
    pub async fn deliver(&self, bundle: Bundle) -> core::result::Result<(), Bundle> {
        self.tx.send(bundle).await.map_err(|SendError(b)| b)
    }

    /// Returns the next bundle queued for delivery, or `None` once the
    /// queue has closed.
    pub async fn next_delivery(&self) -> Option<Bundle> {
        self.rx.recv().await.ok()
    }

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
// The reference back to the table is weak: the BPA owns the table for its
// own lifetime, so a strong reference would only add a cycle through the
// service that holds this sink. A failed upgrade maps to
// `Error::Disconnected`.
struct Sink {
    table: Weak<Table>,
    service_id: ServiceId,
    // The source of every bundle sent through this sink.
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

        // Cancel the stream if the service unregisters: a bundle half sent
        // at that point must not be originated. The token is raced against
        // every pull, so a teardown wakes a stream parked behind a stalled
        // producer. Done here to keep the dispatcher's stream consumers
        // independent of registration state.
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

        // As on the raw sink, a teardown wakes an in-flight send.
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

/// A factory, tagged with the service contract it builds against.
///
/// The two contracts differ only in the factory they carry, so this and a
/// service number are the whole of a declared service.
pub enum FactoryKind<'a> {
    /// A service that receives payloads only.
    Payload(BoxFactory<'a>),
    /// A service that receives whole bundles.
    Raw(raw::BoxFactory<'a>),
}

impl FactoryKind<'_> {
    fn build(self, sink: Arc<Sink>, eid: Eid) -> Result<Kind> {
        match self {
            Self::Payload(factory) => Ok(Kind::Payload(factory.build(Context::new(sink, eid))?)),
            Self::Raw(factory) => Ok(Kind::Raw(factory.build(raw::Context::new(sink, eid))?)),
        }
    }
}

/// A registered service, tagged with the contract it was built against.
///
/// The contracts differ only in delivery, a whole bundle on one side and a
/// payload on the other, which the dispatcher settles by what it hands
/// over. Every other call is dispatched here.
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

/// The first dynamically allocated IPN service number, high enough that
/// allocation does not collide with an explicitly assigned number.
const DYNAMIC_SERVICE_BASE: u32 = 0x8000_0000;

// A service id is claimed, rather than occupied by a record, while its
// registration builds and while its record is torn down.
enum Claim {
    Building,
    Unregistering,
}

// The occupant of a service id: a registered service, or a claim on the
// id.
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

/// The registered services of a BPA.
pub struct Table {
    node_ids: Arc<NodeIds>,
    // The dispatcher outlives this table, so the reference back is weak. A
    // failed upgrade maps to `Error::Disconnected`.
    dispatcher: Weak<Dispatcher>,
    rib: Arc<Rib>,
    services: Mutex<HashMap<ServiceId, Slot>>,
    next_dynamic: AtomicU32,
    tasks: TaskPool,
}

impl Table {
    /// Creates an empty table. Services are added by
    /// [`register`](Self::register) only.
    // `dispatcher` is borrowed because the table keeps only a `Weak` to it.
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
                    // Outstanding claims become teardowns, so a
                    // registration still inside its factory releases its
                    // id instead of filing into a table that is shutting
                    // down.
                    Slot::Claimed(_) => Some((service_id, Slot::Claimed(Claim::Unregistering))),
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

    /// Builds the service, files it under `id`, and calls its `register`.
    ///
    /// When `id` is `None` the BPA allocates a service number. The
    /// factory runs outside the table's lock, against a reservation of the
    /// id, so no other registration can take the id and no caller can
    /// reach a service that is still being built.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ServiceIdInUse`] if the id is taken,
    /// [`Error::AdministrativeEndpoint`] if it names the administrative
    /// endpoint, or the error returned by the factory or by the service's
    /// `register`. A service whose `register` fails is unregistered again,
    /// so a failed call registers nothing.
    pub async fn register(
        self: &Arc<Self>,
        id: Option<ServiceId>,
        factory: FactoryKind<'_>,
    ) -> Result<()> {
        // An allocated service number collides with nothing by
        // construction, but an explicit one may name the administrative
        // endpoint, which is structurally registered (RFC 9758 Section
        // 5.7).
        let service_id =
            id.unwrap_or_else(|| ServiceId::Ipn(self.next_dynamic.fetch_add(1, Ordering::Relaxed)));
        if service_is_admin_endpoint(&service_id) {
            return Err(Error::AdministrativeEndpoint(service_id.to_string()));
        }

        let eid = self.node_ids.resolve_eid(&service_id)?;

        let reservation = self.reserve(&service_id)?;

        // Dropping the reservation releases the id.
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

        self.activate(record).await
    }

    /// Unregisters the service filed under `service_id`, closing its
    /// delivery queue and withdrawing its route.
    pub async fn unregister(&self, service_id: &ServiceId) {
        let record = {
            let mut services = self.services.lock();
            let Some(slot) = services.get_mut(service_id) else {
                return;
            };

            // Claim the id for the teardown, so that nothing registers
            // over a delivery queue that is still draining.
            match core::mem::replace(slot, Slot::Claimed(Claim::Unregistering)) {
                Slot::Attached(record) => record,
                // A registration holds the id. The claim just written
                // tells it to tear the service down instead of filing it.
                Slot::Claimed(_) => return,
            }
        };

        metrics::gauge!("bpa.service.registered").decrement(1.0);
        self.withdraw(record).await;
        self.services.lock().remove(service_id);
    }

    // Claims `service_id`, failing if a service or another claim holds it.
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

    // An id held only by a claim has no record.
    fn record(&self, service_id: &ServiceId) -> Option<Arc<Record>> {
        self.services
            .lock()
            .get(service_id)
            .and_then(Slot::attached)
            .cloned()
    }

    // Starts the service, publishes its route and starts draining its
    // delivery queue. A service that fails to start is unregistered again.
    async fn activate(self: &Arc<Self>, record: Arc<Record>) -> Result<()> {
        let dispatcher = self.dispatcher()?;

        // Start the service before anything reaches it; its queue already
        // holds whatever the channel's creation-time poll recovered.
        if let Err(e) = record.kind.register().await {
            self.unregister(&record.service_id).await;
            return Err(e);
        }

        dispatcher.start_delivery_queue(record.clone());
        let _ = self
            .rib
            .add_service(record.eid.clone(), record.clone())
            .await;

        // Spawned rather than awaited here: a service whose event buffer
        // drains only once registration returns, as the gRPC bridge does,
        // would deadlock registration against its own announcements.
        // Overlapping polls are safe, since each claims its bundles out of
        // WaitingForService with a status CAS.
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

    // Tears a registration down, waking its in-flight streams, closing its
    // queue, withdrawing its route and stopping the service.
    async fn withdraw(&self, record: Arc<Record>) {
        record.cancel.cancel();

        // The consumer finishes its in-flight delivery and exits; queued
        // bundles are swept to WaitingForService by the RIB removal below.
        record.close_queue();

        self.rib.remove_service(&record.eid, record.clone()).await;

        if let Err(e) = record.kind.unregister().await {
            error!("Service {} failed to shut down cleanly: {e}", record.eid);
        }

        info!("Unregistered service: {}", record.eid);
    }
}

// `unregister` decrements the gauge per service; this accounts for the
// services still registered when the table is dropped.
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

// A claim on a service id, released on drop unless `publish` consumes it.
#[must_use = "an unused reservation releases its id immediately"]
struct Reservation<'a> {
    table: &'a Table,
    service_id: &'a ServiceId,
    claimed: bool,
}

impl Reservation<'_> {
    // Files `record` under the reserved id.
    //
    // Fails with `Error::Disconnected` if an `unregister` arrived while the
    // factory ran. The record is then dropped having never been published
    // or started, which completes the teardown.
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
